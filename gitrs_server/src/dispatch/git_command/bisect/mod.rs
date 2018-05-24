mod parse;

use self::parse::{parse_bisect, BisectFinish, BisectOutput, BisectStep};
use error::protocol::SubhandlerError;
use futures::future;
use futures::future::{loop_fn, Future, Loop};
use state;
use std::process;
use std::str;
use tokio_process::CommandExt;
use types::DispatchFuture;
use util::git;
use util::transport::{read_message, send_message};

// See https://github.com/rust-lang/rfcs/issues/2407#issuecomment-385291238.
macro_rules! enclose {
    (($($x:ident),*) $y:expr) => {
        {
            $(let $x = $x.clone();)*
                $y
        }
    };
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum InboundMessage {
    Bad,
    Good,
}

#[derive(Debug, Serialize)]
#[serde(tag = "reason")]
enum BisectError {
    RepoPathNotSet,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OutboundMessage {
    Step(BisectStep),
    Error(BisectError),
    Finish(BisectFinish),
}

type CommandBuilder = Box<Fn() -> process::Command + Send>;

fn verify_repo_path(repo_path: Option<String>) -> Result<String, SubhandlerError<BisectError>> {
    use self::BisectError::RepoPathNotSet;

    match repo_path {
        Some(repo_path) => Ok(repo_path),
        None => Err(SubhandlerError::Subhandler(RepoPathNotSet)),
    }
}

fn run_command(build_command: CommandBuilder) -> impl Future<Item = String, Error = SubhandlerError<BisectError>> {
    use error::protocol::Error::Process;
    use error::protocol::ProcessError::{Encoding, Failed};

    build_command()
        .output_async()
        .map_err(|_| SubhandlerError::Shared(Process(Failed)))
        .and_then(|output| {
            str::from_utf8(&output.stdout)
                .map(String::from)
                .map_err(|_| SubhandlerError::Shared(Process(Encoding)))
        })
}

fn continue_bisect(
    build_command_bad: CommandBuilder,
    build_command_good: CommandBuilder,
    connection_state: state::Connection,
) -> impl Future<Item = (CommandBuilder, state::Connection), Error = (SubhandlerError<BisectError>, state::Connection)>
{
    read_message(connection_state)
        .map_err(|(err, connection_state)| (SubhandlerError::Shared(err), connection_state))
        .map(|(message, connection_state)| {
            (
                match message {
                    InboundMessage::Bad => build_command_bad,
                    InboundMessage::Good => build_command_good,
                },
                connection_state,
            )
        })
}

fn finish_bisect(
    bisect_finish: BisectFinish,
    build_command_reset: CommandBuilder,
    connection_state: state::Connection,
) -> impl Future<Item = state::Connection, Error = (SubhandlerError<BisectError>, state::Connection)> {
    run_command(build_command_reset).then(|result| -> Box<Future<Item = _, Error = _> + Send> {
        match result {
            Ok(_) => Box::new(
                send_message(connection_state, OutboundMessage::Finish(bisect_finish))
                    .map_err(|(err, connection_state)| (SubhandlerError::Shared(err), connection_state)),
            ),
            Err(err) => Box::new(future::err((err, connection_state))),
        }
    })
}

fn handle_errors((err, connection_state): (SubhandlerError<BisectError>, state::Connection)) -> DispatchFuture {
    match err {
        SubhandlerError::Shared(err) => Box::new(future::err((err, connection_state))) as DispatchFuture,
        SubhandlerError::Subhandler(err) => {
            Box::new(send_message(connection_state, OutboundMessage::Error(err))) as DispatchFuture
        }
    }
}

fn build_bisect_step_handler(
    repo_path: String,
) -> impl FnOnce(
    (String, state::Connection)
) -> Box<
    Future<
            Item = Loop<state::Connection, (CommandBuilder, state::Connection)>,
            Error = (SubhandlerError<BisectError>, state::Connection),
        >
        + Send,
> {
    use error::protocol::Error::Process;
    use error::protocol::ProcessError::Parsing;

    let build_command_good = enclose! { (repo_path) move || {
        let mut command = git::new_command_with_repo_path(&repo_path);
        command.arg("bisect").arg("good");
        command
    }};

    let build_command_bad = enclose! { (repo_path) move || {
        let mut command = git::new_command_with_repo_path(&repo_path);
        command.arg("bisect").arg("bad");
        command
    }};

    let build_command_reset = enclose! { (repo_path) move || {
        let mut command = git::new_command_with_repo_path(&repo_path);
        command.arg("bisect").arg("reset");
        command
    }};

    enclose! { (build_command_bad, build_command_good, build_command_reset)
        move |(output, connection_state): (String, state::Connection)| -> Box<Future<Item = _, Error = _> + Send> {
            match parse_bisect(&output[..]) {
                Ok((_, output)) => match output {
                    BisectOutput::Finish(bisect_finish) => Box::new(finish_bisect(
                        bisect_finish,
                        Box::new(build_command_reset),
                        connection_state).map(Loop::Break)
                    ),
                    BisectOutput::Step(bisect_step) => Box::new(
                        send_message(connection_state, OutboundMessage::Step(bisect_step))
                            .map_err(|(err, connection_state)| (
                                SubhandlerError::Shared(err),
                                connection_state
                            ))
                            .and_then(|connection_state| {
                                continue_bisect(
                                    Box::new(build_command_bad),
                                    Box::new(build_command_good),
                                    connection_state
                                )
                            })
                            .map(Loop::Continue)
                    )
                },
                Err(_) => Box::new(future::err((SubhandlerError::Shared(Process(Parsing)), connection_state))),
            }
        }
    }
}

pub fn dispatch(connection_state: state::Connection, bad: String, good: String) -> DispatchFuture {
    Box::new(
        future::result(match verify_repo_path(connection_state.repo_path.clone()) {
            Ok(repo_path) => Ok((repo_path, connection_state)),
            Err(err) => Err((err, connection_state)),
        }).and_then(move |(repo_path, connection_state)| {
            let build_command_start: CommandBuilder = Box::new(enclose! { (repo_path) move || {
                let mut command = git::new_command_with_repo_path(&repo_path);
                command
                    .arg("bisect")
                    .arg("start")
                    .arg(bad.clone())
                    .arg(good.clone())
                    .arg("--");
                command
            }});

            loop_fn(
                (build_command_start, connection_state),
                enclose! { (repo_path)
                    move |(build_command, connection_state)| {
                        run_command(build_command)
                            .then(|result| match result {
                                Ok(stdout) => future::ok((stdout, connection_state)),
                                Err(err) => future::err((err, connection_state)),
                            })
                            .and_then(build_bisect_step_handler(repo_path.clone()))
                    }
                },
            )
        })
            .or_else(handle_errors),
    )
}
