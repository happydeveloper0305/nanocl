use std::collections::HashMap;

use futures::StreamExt;
use indicatif::{ProgressBar, MultiProgress, ProgressStyle};

use bollard_next::{Docker, API_DEFAULT_VERSION};
use bollard_next::image::CreateImageOptions;
use bollard_next::container::CreateContainerOptions;
use bollard_next::service::{
  HostConfig, ProgressDetail, RestartPolicy, RestartPolicyNameEnum,
  ContainerCreateResponse,
};

use nanocl_error::io::{IoError, FromIo, IoResult};

use nanocld_client::stubs::cargo_spec::{CargoSpecPartial, Config};

use crate::utils::hash;
use crate::utils::math::calculate_percentage;
use crate::models::DockerContextMeta;

/// ## Update image progress
///
/// Update progress bar for install image
///
/// ## Arguments
///
/// * [multiprogress](MultiProgress) The multiprogress bar
/// * [layers](HashMap) The layers
/// * [id](str) The id of the layer
/// * [progress](ProgressDetail) The progress detail
///
fn update_image_progress(
  multiprogress: &MultiProgress,
  layers: &mut HashMap<String, ProgressBar>,
  id: &str,
  progress: &ProgressDetail,
) {
  let total: u64 = progress
    .total
    .unwrap_or_default()
    .try_into()
    .unwrap_or_default();
  let current: u64 = progress
    .current
    .unwrap_or_default()
    .try_into()
    .unwrap_or_default();
  if let Some(pg) = layers.get(id) {
    let percent = calculate_percentage(current, total);
    pg.set_position(percent);
  } else {
    let pg = ProgressBar::new(100);
    let style = ProgressStyle::with_template(
      "[{elapsed_precise}] [{bar:20.cyan/blue}] {pos:>7}% {msg}",
    )
    .unwrap()
    .progress_chars("=> ");
    pg.set_style(style);
    multiprogress.add(pg.to_owned());
    let percent = calculate_percentage(current, total);
    pg.set_position(percent);
    layers.insert(id.to_owned(), pg);
  }
}

/// ## Install image
///
/// Install image directly with docker and output progress
///
/// ## Arguments
///
/// * [from_image](str) The image to install
/// * [tag](str) The tag of the image
/// * [docker_api](Docker) The docker client
///
pub async fn install_image(
  from_image: &str,
  tag: &str,
  docker_api: &Docker,
) -> IoResult<()> {
  let options = Some(CreateImageOptions {
    from_image,
    tag,
    ..Default::default()
  });

  let mut stream = docker_api.create_image(options, None, None);
  let mut layers: HashMap<String, ProgressBar> = HashMap::new();
  let multiprogress = MultiProgress::new();
  multiprogress.set_move_cursor(false);
  while let Some(res) = stream.next().await {
    let data = res
      .map_err(|err| err.map_err_context(|| "Install image stream failed"))?;
    let status = data.status.unwrap_or_default();
    let id = data.id.unwrap_or_default();
    let progress = data.progress_detail.unwrap_or_default();
    match status.as_str() {
      "Pulling fs layer" => {
        update_image_progress(&multiprogress, &mut layers, &id, &progress);
      }
      "Downloading" => {
        update_image_progress(&multiprogress, &mut layers, &id, &progress);
      }
      "Download complete" => {
        if let Some(pg) = layers.get(&id) {
          pg.set_position(100);
        }
      }
      "Extracting" => {
        update_image_progress(&multiprogress, &mut layers, &id, &progress);
      }
      _ => {
        if layers.get(&id).is_none() {
          let _ = multiprogress.println(&status);
        }
      }
    };
    if let Some(pg) = layers.get(&id) {
      pg.set_message(format!("[{}] {}", &id, &status));
    }
  }

  Ok(())
}

/// ## Connect
///
/// Generate a docker client from the docker host
///
/// ## Arguments
///
/// * [docker_host](str) The docker host
///
/// ## Return
///
/// [IoResult](IoResult) containing a [Docker](Docker)
///
pub fn connect(docker_host: &str) -> IoResult<Docker> {
  let docker = match &docker_host {
    docker_host if docker_host.starts_with("unix://") => {
      let path = docker_host.trim_start_matches("unix://");
      if !std::path::Path::new(&path).exists() {
        return Err(IoError::not_found(
          "Unix socket file can't be found at",
          path,
        ));
      }
      Docker::connect_with_unix(path, 120, API_DEFAULT_VERSION).map_err(
        |err| {
          err.map_err_context(|| {
            format!("Unable to connect to docker at {path}")
          })
        },
      )?
    }
    docker_host
      if docker_host.starts_with("http://")
        | docker_host.starts_with("https://") =>
    {
      Docker::connect_with_http(docker_host, 120, API_DEFAULT_VERSION).map_err(
        |err| {
          err.map_err_context(|| {
            format!("Unable to connect to docker at {docker_host}")
          })
        },
      )?
    }
    _ => {
      return Err(IoError::invalid_data(
        "Url",
        &format!("{docker_host} have invalid schema"),
      ))
    }
  };

  Ok(docker)
}

/// ## Hook labels
///
/// Hook labels for a container
///
/// ## Arguments
///
/// * [key](str) The key of the container
/// * [namespace](str) The namespace of the container
/// * [labels](HashMap<String, String>) The labels of the container
///
/// ## Return
///
/// [HashMap](HashMap) of [String](String) to [String](String)
///
pub fn hook_labels(
  key: &str,
  namespace: &str,
  labels: &HashMap<String, String>,
) -> HashMap<String, String> {
  let mut hooked_labels = labels.clone();
  hooked_labels.insert("io.nanocl".to_owned(), "enabled".to_owned());
  hooked_labels.insert("io.nanocl.kind".to_owned(), "Cargo".to_owned());
  hooked_labels.insert("io.nanocl.c".to_owned(), key.to_owned());
  hooked_labels.insert("io.nanocl.n".to_owned(), namespace.to_owned());
  hooked_labels.insert(
    "com.docker.compose.project".to_owned(),
    format!("nanocl_{namespace}"),
  );
  hooked_labels
}

/// ## Create cargo container
///
/// Create a container from a cargo config
///
/// ## Arguments
///
/// * [cargo](CargoSpecPartial) The cargo config
/// * [namespace](str) The namespace of the container
/// * [docker](Docker) The docker client
///
/// ## Return
///
/// [IoResult](IoResult) containing a [ContainerCreateResponse](ContainerCreateResponse)
///
pub async fn create_cargo_container(
  cargo: &CargoSpecPartial,
  namespace: &str,
  docker: &Docker,
) -> IoResult<ContainerCreateResponse> {
  let name = &cargo.name;
  let config = &cargo.container;
  let key = format!("{name}.{namespace}");
  let hooked_config = Config {
    labels: Some(hook_labels(
      &key,
      namespace,
      &config.labels.clone().unwrap_or_default(),
    )),
    host_config: Some(HostConfig {
      restart_policy: Some(RestartPolicy {
        name: Some(RestartPolicyNameEnum::ALWAYS),
        ..Default::default()
      }),
      ..config.host_config.clone().unwrap_or_default()
    }),
    ..config.clone()
  };
  let container = docker
    .create_container(
      Some(CreateContainerOptions {
        name: format!("{key}.c"),
        platform: None,
      }),
      hooked_config,
    )
    .await
    .map_err(|err| err.map_err_context(|| format!("Cargo {name}")))?;

  Ok(container)
}

/// ## Detect docker host
///
/// Detect docker host from docker config
///
/// ## Return
///
/// [IoResult](IoResult) containing a tuple of [String](String) and a [bool](bool)
///
pub fn detect_docker_host() -> IoResult<(String, bool)> {
  let home = std::env::var("HOME").map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::Other, "Could not get $HOME")
  })?;
  let path = format!("{home}/.docker/config.json");
  let Ok(str) = std::fs::read_to_string(path) else {
    return Ok(("unix:///var/run/docker.sock".into(), false));
  };
  let config =
    serde_json::from_str::<serde_json::Value>(&str).map_err(|err| {
      std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("Could not parse docker config: {err}"),
      )
    })?;
  let context = config["currentContext"].as_str().unwrap_or("default");
  if context == "default" {
    return Ok(("unix:///var/run/docker.sock".into(), false));
  }
  let hash = hash::calculate_SHA256(context);
  let path = format!("{home}/.docker/contexts/meta/{hash}/meta.json",);
  let str = std::fs::read_to_string(path)?;
  let config =
    serde_json::from_str::<DockerContextMeta>(&str).map_err(|err| {
      std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("Could not parse docker config: {err}"),
      )
    })?;
  let Some(endpoint) = config.endpoints.get("docker") else {
    return Ok(("unix:///var/run/docker.sock".into(), false));
  };
  if !endpoint.host.starts_with("unix://") {
    return Err(
      std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("No unix docker endpoint unsupported yet: {}", endpoint.host),
      )
      .into(),
    );
  }
  if context == "desktop-linux" {
    return Ok((endpoint.host.to_owned(), true));
  }
  Ok((endpoint.host.to_owned(), false))
}
