// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{
  configure_cargo, detect_target_ok, ensure_init, env, get_app, get_config, inject_resources,
  load_pbxproj, log_finished, merge_plist, open_and_wait, project_config,
  synchronize_project_config, MobileTarget, OptionsHandle,
};
use crate::{
  build::Options as BuildOptions,
  helpers::{
    app_paths::tauri_dir,
    config::{get as get_tauri_config, ConfigHandle},
    flock,
  },
  interface::{AppInterface, AppSettings, Interface, Options as InterfaceOptions},
  mobile::{write_options, CliOptions},
  ConfigValue, Result,
};
use clap::{ArgAction, Parser, ValueEnum};

use anyhow::Context;
use cargo_mobile2::{
  apple::{
    config::Config as AppleConfig,
    target::{ExportConfig, Target},
  },
  env::Env,
  opts::{NoiseLevel, Profile},
  target::{call_for_targets_with_fallback, TargetInvalid, TargetTrait},
};

use std::{
  env::{set_current_dir, var, var_os},
  fs,
  path::PathBuf,
};

#[derive(Debug, Clone, Parser)]
#[clap(
  about = "Build your app in release mode for iOS and generate IPAs",
  long_about = "Build your app in release mode for iOS and generate IPAs. It makes use of the `build.frontendDist` property from your `tauri.conf.json` file. It also runs your `build.beforeBuildCommand` which usually builds your frontend into `build.frontendDist`."
)]
pub struct Options {
  /// Builds with the debug flag
  #[clap(short, long)]
  pub debug: bool,
  /// Which targets to build.
  #[clap(
    short,
    long = "target",
    action = ArgAction::Append,
    num_args(0..),
    default_value = Target::DEFAULT_KEY,
    value_parser(clap::builder::PossibleValuesParser::new(Target::name_list()))
  )]
  pub targets: Vec<String>,
  /// List of cargo features to activate
  #[clap(short, long, action = ArgAction::Append, num_args(0..))]
  pub features: Option<Vec<String>>,
  /// JSON string or path to JSON file to merge with tauri.conf.json
  #[clap(short, long)]
  pub config: Option<ConfigValue>,
  /// Build number to append to the app version.
  #[clap(long)]
  pub build_number: Option<u32>,
  /// Open Xcode
  #[clap(short, long)]
  pub open: bool,
  /// Skip prompting for values
  #[clap(long, env = "CI")]
  pub ci: bool,
  /// Describes how Xcode should export the archive.
  ///
  /// Use this to create a package ready for the App Store (app-store-connect option) or TestFlight (release-testing option).
  #[clap(long, value_enum)]
  pub export_method: Option<ExportMethod>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ExportMethod {
  AppStoreConnect,
  ReleaseTesting,
  Debugging,
}

impl std::fmt::Display for ExportMethod {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::AppStoreConnect => write!(f, "app-store-connect"),
      Self::ReleaseTesting => write!(f, "release-testing"),
      Self::Debugging => write!(f, "debugging"),
    }
  }
}

impl std::str::FromStr for ExportMethod {
  type Err = &'static str;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "app-store-connect" => Ok(Self::AppStoreConnect),
      "release-testing" => Ok(Self::ReleaseTesting),
      "debugging" => Ok(Self::Debugging),
      _ => Err("unknown ios target"),
    }
  }
}

impl From<Options> for BuildOptions {
  fn from(options: Options) -> Self {
    Self {
      runner: None,
      debug: options.debug,
      target: None,
      features: options.features,
      bundles: None,
      no_bundle: false,
      config: options.config,
      args: Vec::new(),
      ci: options.ci,
    }
  }
}

pub fn command(options: Options, noise_level: NoiseLevel) -> Result<()> {
  crate::helpers::app_paths::resolve();

  let mut build_options: BuildOptions = options.clone().into();
  build_options.target = Some(
    Target::all()
      .get(
        options
          .targets
          .first()
          .map(|t| t.as_str())
          .unwrap_or(Target::DEFAULT_KEY),
      )
      .unwrap()
      .triple
      .into(),
  );

  let tauri_config = get_tauri_config(
    tauri_utils::platform::Target::Ios,
    options.config.as_ref().map(|c| &c.0),
  )?;
  let (interface, app, mut config) = {
    let tauri_config_guard = tauri_config.lock().unwrap();
    let tauri_config_ = tauri_config_guard.as_ref().unwrap();

    let interface = AppInterface::new(tauri_config_, build_options.target.clone())?;
    interface.build_options(&mut Vec::new(), &mut build_options.features, true);

    let app = get_app(MobileTarget::Ios, tauri_config_, &interface);
    let (config, _metadata) = get_config(
      &app,
      tauri_config_,
      build_options.features.as_ref(),
      &Default::default(),
    );
    (interface, app, config)
  };

  let tauri_path = tauri_dir();
  set_current_dir(tauri_path).with_context(|| "failed to change current working directory")?;

  ensure_init(
    &tauri_config,
    config.app(),
    config.project_dir(),
    MobileTarget::Ios,
  )?;
  inject_resources(&config, tauri_config.lock().unwrap().as_ref().unwrap())?;

  let info_plist_path = config
    .project_dir()
    .join(config.scheme())
    .join("Info.plist");
  let merged_info_plist = merge_plist(vec![
    info_plist_path.clone().into(),
    tauri_path.join("Info.plist").into(),
    tauri_path.join("Info.ios.plist").into(),
  ])?;
  merged_info_plist.to_file_xml(&info_plist_path)?;

  let mut env = env()?;
  configure_cargo(&app, None)?;

  let mut export_options_plist = plist::Dictionary::new();
  if let Some(method) = options.export_method {
    export_options_plist.insert("method".to_string(), method.to_string().into());
  }

  let (keychain, provisioning_profile) = super::signing_from_env()?;
  let project_config = project_config(keychain.as_ref(), provisioning_profile.as_ref())?;
  let mut pbxproj = load_pbxproj(&config)?;

  // synchronize pbxproj and exportoptions
  synchronize_project_config(
    &app,
    &mut pbxproj,
    &mut export_options_plist,
    &project_config,
    options.debug,
  )?;
  if pbxproj.has_changes() {
    pbxproj.save()?;
  }

  // merge export options and write to temp file
  let _export_options_tmp = if !export_options_plist.is_empty() {
    let export_options_plist_path = config.project_dir().join("ExportOptions.plist");
    let export_options = tempfile::NamedTempFile::new()?;

    let merged_plist = merge_plist(vec![
      export_options.path().to_owned().into(),
      export_options_plist_path.clone().into(),
      plist::Value::from(export_options_plist).into(),
    ])?;
    merged_plist.to_file_xml(export_options.path())?;

    config.set_export_options_plist_path(export_options.path());

    Some(export_options)
  } else {
    None
  };

  let open = options.open;
  let _handle = run_build(
    interface,
    options,
    build_options,
    tauri_config,
    &config,
    &mut env,
    noise_level,
  )?;

  if open {
    open_and_wait(&config, &env);
  }

  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_build(
  interface: AppInterface,
  options: Options,
  mut build_options: BuildOptions,
  tauri_config: ConfigHandle,
  config: &AppleConfig,
  env: &mut Env,
  noise_level: NoiseLevel,
) -> Result<OptionsHandle> {
  let profile = if options.debug {
    Profile::Debug
  } else {
    Profile::Release
  };

  crate::build::setup(&interface, &mut build_options, tauri_config.clone(), true)?;

  let app_settings = interface.app_settings();
  let bin_path = app_settings.app_binary_path(&InterfaceOptions {
    debug: build_options.debug,
    target: build_options.target.clone(),
    ..Default::default()
  })?;
  let out_dir = bin_path.parent().unwrap();
  let _lock = flock::open_rw(out_dir.join("lock").with_extension("ios"), "iOS")?;

  let cli_options = CliOptions {
    dev: false,
    features: build_options.features.clone(),
    args: build_options.args.clone(),
    noise_level,
    vars: Default::default(),
    config: build_options.config.clone(),
    target_device: None,
  };
  let handle = write_options(
    &tauri_config.lock().unwrap().as_ref().unwrap().identifier,
    cli_options,
  )?;

  let mut out_files = Vec::new();

  call_for_targets_with_fallback(
    options.targets.iter(),
    &detect_target_ok,
    env,
    |target: &Target| -> Result<()> {
      let mut app_version = config.bundle_version().clone();
      if let Some(build_number) = options.build_number {
        app_version.push_extra(build_number);
      }

      target.build(config, env, NoiseLevel::FranklyQuitePedantic, profile)?;
      target.archive(config, env, noise_level, profile, Some(app_version))?;

      let mut export_config = ExportConfig::new().allow_provisioning_updates();
      if let Some(credentials) = auth_credentials_from_env()? {
        export_config = export_config.authentication_credentials(credentials);
      }

      target.export(config, env, noise_level, export_config)?;

      if let Ok(ipa_path) = config.ipa_path() {
        let out_dir = config.export_dir().join(target.arch);
        fs::create_dir_all(&out_dir)?;
        let path = out_dir.join(ipa_path.file_name().unwrap());
        fs::rename(&ipa_path, &path)?;
        out_files.push(path);
      }

      Ok(())
    },
  )
  .map_err(|e: TargetInvalid| anyhow::anyhow!(e.to_string()))??;

  log_finished(out_files, "IPA");

  Ok(handle)
}

fn auth_credentials_from_env() -> Result<Option<cargo_mobile2::apple::target::AuthCredentials>> {
  match (
    var("APPLE_API_KEY"),
    var("APPLE_API_ISSUER"),
    var_os("APPLE_API_KEY_PATH").map(PathBuf::from),
  ) {
    (Ok(key_id), Ok(key_issuer_id), Some(key_path)) => {
      Ok(Some(cargo_mobile2::apple::target::AuthCredentials {
        key_path,
        key_id,
        key_issuer_id,
      }))
    }
    (Err(_), Err(_), None) => Ok(None),
    _ => anyhow::bail!(
      "APPLE_API_KEY, APPLE_API_ISSUER and APPLE_API_KEY_PATH must be provided for code signing"
    ),
  }
}
