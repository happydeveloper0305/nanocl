use std::process;
use std::collections::HashMap;

use ntex::rt;
use futures::channel::mpsc;
use futures::{StreamExt, SinkExt};
use futures::stream::FuturesUnordered;
use bollard_next::exec::{CreateExecOptions, StartExecOptions};

use nanocl_error::io::{FromIo, IoResult};
use nanocld_client::stubs::{
  process::{OutputKind, ProcessLogQuery, ProcessStatsQuery},
  generic::{GenericFilter, GenericListNspQuery},
  cargo::{CargoDeleteQuery, CargoSummary},
};

use crate::{
  utils,
  config::CliConfig,
  models::{
    CargoArg, CargoCreateOpts, CargoCommand, CargoRemoveOpts, CargoRow,
    CargoStartOpts, CargoStopOpts, CargoPatchOpts, CargoInspectOpts,
    CargoExecOpts, CargoHistoryOpts, CargoRevertOpts, CargoLogsOpts,
    CargoRunOpts, CargoRestartOpts, CargoStatsOpts, ProcessStatsRow,
  },
};

use super::GenericList;

impl GenericList for CargoArg {
  type Item = CargoRow;
  type Args = CargoArg;
  type ApiItem = CargoSummary;

  fn object_name() -> &'static str {
    "cargoes"
  }

  fn get_key(item: &Self::Item) -> String {
    item.name.clone()
  }

  fn transform_filter(
    args: &Self::Args,
    filter: &GenericFilter,
  ) -> impl serde::Serialize {
    GenericListNspQuery::try_from(filter.clone())
      .unwrap()
      .with_namespace(args.namespace.as_deref())
  }
}

/// Execute the `nanocl cargo create` command to create a new cargo
async fn exec_cargo_create(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoCreateOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let cargo = opts.clone().into();
  let item = client
    .create_cargo(&cargo, args.namespace.as_deref())
    .await?;
  println!("{}", &item.spec.cargo_key);
  Ok(())
}

/// Execute the `nanocl cargo rm` command to remove a cargo
async fn exec_cargo_rm(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoRemoveOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  if !opts.skip_confirm {
    utils::dialog::confirm(&format!("Delete cargo  {}?", opts.names.join(",")))
      .map_err(|err| err.map_err_context(|| "Delete cargo"))?;
  }
  let query = CargoDeleteQuery {
    namespace: args.namespace.clone(),
    force: Some(opts.force),
  };
  for name in &opts.names {
    if let Err(err) = client.delete_cargo(name, Some(&query)).await {
      eprintln!("{name}: {err}");
    }
  }
  Ok(())
}

/// Execute the `nanocl cargo start` command to start a cargo
async fn exec_cargo_start(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoStartOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  client
    .start_process("cargo", &opts.name, args.namespace.as_deref())
    .await?;
  Ok(())
}

/// Execute the `nanocl cargo stop` command to stop a cargo
async fn exec_cargo_stop(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoStopOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  for name in &opts.names {
    if let Err(err) = client
      .stop_process("cargo", name, args.namespace.as_deref())
      .await
    {
      eprintln!("{name}: {err}");
    }
  }
  Ok(())
}

/// Execute the `nanocl cargo restart` command to restart a cargo
async fn exec_cargo_restart(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoRestartOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  for name in &opts.names {
    client
      .restart_process("cargo", name, args.namespace.as_deref())
      .await?;
  }
  Ok(())
}

/// Execute the `nanocl cargo patch` command to patch a cargo
async fn exec_cargo_patch(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoPatchOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  client
    .patch_cargo(&opts.name, &opts.clone().into(), args.namespace.as_deref())
    .await?;
  Ok(())
}

/// Execute the `nanocl cargo inspect` command to inspect a cargo
async fn exec_cargo_inspect(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoInspectOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let cargo = client
    .inspect_cargo(&opts.name, args.namespace.as_deref())
    .await?;
  let display = opts
    .display
    .clone()
    .unwrap_or(cli_conf.user_config.display_format.clone());
  utils::print::display_format(&display, cargo)?;
  Ok(())
}

/// Execute the `nanocl cargo exec` command to execute a command in a cargo
async fn exec_cargo_exec(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoExecOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let exec: CreateExecOptions = opts.clone().into();
  let result = client
    .create_exec(&opts.name, &exec, args.namespace.as_deref())
    .await?;
  let mut stream = client
    .start_exec(
      &result.id,
      &StartExecOptions {
        tty: opts.tty,
        ..Default::default()
      },
    )
    .await?;
  while let Some(output) = stream.next().await {
    let output = output?;
    match output.kind {
      OutputKind::StdOut => {
        print!("{}", &output.data);
      }
      OutputKind::StdErr => {
        eprint!("{}", output.data);
      }
      OutputKind::StdIn => println!("TODO: StdIn {}", &output.data),
      OutputKind::Console => print!("{}", &output.data),
    }
  }
  let exec_infos = client.inspect_exec(&result.id).await?;
  match exec_infos.exit_code {
    Some(code) => {
      if code == 0 {
        return Ok(());
      }
      process::exit(code.try_into().unwrap_or(1))
    }
    None => Ok(()),
  }
}

/// Execute the `nanocl cargo history` command to list the history of a cargo
async fn exec_cargo_history(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoHistoryOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let histories = client
    .list_history_cargo(&opts.name, args.namespace.as_deref())
    .await?;
  utils::print::print_yml(histories)?;
  Ok(())
}

/// Execute the `nanocl cargo logs` command to list the logs of a cargo
async fn exec_cargo_logs(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoLogsOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let query = ProcessLogQuery {
    namespace: args.namespace.clone(),
    tail: opts.tail.clone(),
    since: opts.since,
    until: opts.until,
    follow: Some(opts.follow),
    timestamps: Some(opts.timestamps),
    stderr: None,
    stdout: None,
  };
  let stream = client
    .logs_processes("cargo", &opts.name, Some(&query))
    .await?;
  utils::print::logs_process_stream(stream).await?;
  Ok(())
}

/// Execute the `nanocl cargo stats` command to list the stats of a cargo
async fn exec_cargo_stats(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoStatsOpts,
) -> IoResult<()> {
  let client = cli_conf.client.clone();
  let query = ProcessStatsQuery {
    namespace: args.namespace.clone(),
    stream: if opts.no_stream { Some(false) } else { None },
    one_shot: Some(false),
  };
  let mut stats_cargoes = HashMap::new();
  let (tx, mut rx) = mpsc::unbounded();
  let futures = opts
    .names
    .iter()
    .map(|name| {
      let name = name.clone();
      let query = query.clone();
      let mut tx = tx.clone();
      let client = client.clone();
      async move {
        let Ok(mut stream) =
          client.stats_processes("cargo", &name, Some(&query)).await
        else {
          return;
        };
        while let Some(stats) = stream.next().await {
          let stats = match stats {
            Ok(stats) => stats,
            Err(e) => {
              eprintln!("Error: {e}");
              break;
            }
          };
          if let Err(err) = tx.send(stats).await {
            eprintln!("Error: {err}");
            break;
          }
        }
      }
    })
    .collect::<FuturesUnordered<_>>()
    .collect::<Vec<_>>();
  rt::spawn(futures);
  while let Some(stats) = rx.next().await {
    stats_cargoes.insert(stats.name.clone(), stats.clone());
    // convert stats_cargoes in a Arrays of CargoStatsRow
    let stats = stats_cargoes
      .values()
      .map(|stats| ProcessStatsRow::from(stats.clone()))
      .collect::<Vec<ProcessStatsRow>>();
    // clear terminal
    let term = dialoguer::console::Term::stdout();
    let _ = term.clear_screen();
    utils::print::print_table(stats);
  }
  Ok(())
}

/// Execute the `nanocl cargo revert` command to revert a cargo to a previous state
async fn exec_cargo_revert(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoRevertOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let cargo = client
    .revert_cargo(&opts.name, &opts.history_id, args.namespace.as_deref())
    .await?;
  utils::print::print_yml(cargo)?;
  Ok(())
}

/// Execute the `nanocl cargo run` command to run a cargo
async fn exec_cargo_run(
  cli_conf: &CliConfig,
  args: &CargoArg,
  opts: &CargoRunOpts,
) -> IoResult<()> {
  let client = &cli_conf.client;
  let cargo = client
    .create_cargo(&opts.clone().into(), args.namespace.as_deref())
    .await?;
  client
    .start_process("cargo", &cargo.spec.name, Some(&cargo.namespace_name))
    .await?;
  Ok(())
}

/// Function that execute when running `nanocl cargo`
pub async fn exec_cargo(cli_conf: &CliConfig, args: &CargoArg) -> IoResult<()> {
  match &args.command {
    CargoCommand::List(opts) => {
      CargoArg::exec_ls(&cli_conf.client, args, opts).await
    }
    CargoCommand::Create(opts) => exec_cargo_create(cli_conf, args, opts).await,
    CargoCommand::Remove(opts) => exec_cargo_rm(cli_conf, args, opts).await,
    CargoCommand::Start(opts) => exec_cargo_start(cli_conf, args, opts).await,
    CargoCommand::Stop(opts) => exec_cargo_stop(cli_conf, args, opts).await,
    CargoCommand::Patch(opts) => exec_cargo_patch(cli_conf, args, opts).await,
    CargoCommand::Inspect(opts) => {
      exec_cargo_inspect(cli_conf, args, opts).await
    }
    CargoCommand::Exec(opts) => exec_cargo_exec(cli_conf, args, opts).await,
    CargoCommand::History(opts) => {
      exec_cargo_history(cli_conf, args, opts).await
    }
    CargoCommand::Revert(opts) => exec_cargo_revert(cli_conf, args, opts).await,
    CargoCommand::Logs(opts) => exec_cargo_logs(cli_conf, args, opts).await,
    CargoCommand::Run(opts) => exec_cargo_run(cli_conf, args, opts).await,
    CargoCommand::Restart(opts) => {
      exec_cargo_restart(cli_conf, args, opts).await
    }
    CargoCommand::Stats(opts) => exec_cargo_stats(cli_conf, args, opts).await,
  }
}
