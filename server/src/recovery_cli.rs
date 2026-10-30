//! Offline recovery restore command. It never opens the configured server store.

use crate::errors::AtomicServerResult;
use clap::{CommandFactory, Parser, Subcommand};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

#[derive(Parser)]
#[command(name = "atomic-server")]
struct RecoveryCli {
    #[command(subcommand)]
    command: RecoveryCliCommand,
}

#[derive(Subcommand)]
enum RecoveryCliCommand {
    #[command(subcommand)]
    Recovery(crate::config::RecoveryCommand),
}

pub async fn run_if_requested() -> Option<AtomicServerResult<()>> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if !has_recovery_subcommand(&args) {
        return None;
    }
    Some(run_from_args(&args).await)
}

fn has_recovery_subcommand(args: &[OsString]) -> bool {
    let command = crate::config::Opts::command();
    let mut index = 1;
    while index < args.len() {
        let Some(value) = args[index].to_str() else {
            index += 1;
            continue;
        };
        if value.starts_with("--") {
            let (option, inline_value) = value
                .split_once('=')
                .map_or((value, false), |(option, _)| (option, true));
            let takes_value = command.get_arguments().any(|argument| {
                argument.get_action().takes_values()
                    && argument.get_long() == Some(option.trim_start_matches("--"))
            });
            index += usize::from(takes_value && !inline_value) + 1;
            continue;
        }
        if value.starts_with('-') {
            let mut characters = value.chars();
            let _ = characters.next();
            if let Some(short) = characters.next() {
                let takes_value = command.get_arguments().any(|argument| {
                    argument.get_action().takes_values() && argument.get_short() == Some(short)
                });
                let inline_value = characters.next().is_some();
                index += usize::from(takes_value && !inline_value) + 1;
            } else {
                index += 1;
            }
            continue;
        }
        return value == "recovery";
    }
    false
}

async fn run_from_args(args: &[OsString]) -> AtomicServerResult<()> {
    let protected = protected_paths(args)?;
    let filtered = filter_directory_overrides(args);
    let cli = RecoveryCli::try_parse_from(filtered)
        .map_err(|error| format!("Invalid recovery command: {error}"))?;
    match cli.command {
        RecoveryCliCommand::Recovery(crate::config::RecoveryCommand::Restore(options)) => {
            restore(&options.bundle, &options.target_dir, &protected).await
        }
    }
}

fn filter_directory_overrides(args: &[OsString]) -> Vec<OsString> {
    let mut filtered = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        filtered.push(program.clone());
    }
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_str().unwrap_or_default();
        if ["--data-dir", "--config-dir", "--cache-dir"].contains(&value) {
            index = (index + 2).min(args.len());
            continue;
        }
        if ["--data-dir=", "--config-dir=", "--cache-dir="]
            .iter()
            .any(|name| value.starts_with(name))
        {
            index += 1;
            continue;
        }
        filtered.push(args[index].clone());
        index += 1;
    }
    filtered
}

fn protected_paths(args: &[OsString]) -> AtomicServerResult<Vec<PathBuf>> {
    let project_dirs = directories::ProjectDirs::from("", "", "atomic-data");
    let mut protected = Vec::new();
    let data_dir = directory_override(args, "--data-dir")
        .or_else(|| std::env::var_os("ATOMIC_DATA_DIR").map(PathBuf::from))
        .or_else(|| project_dirs.as_ref().map(|dirs| dirs.data_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("atomic-data/data"));
    protected.push(data_dir);
    if let Some(config_dir) = directory_override(args, "--config-dir")
        .or_else(|| std::env::var_os("ATOMIC_CONFIG_DIR").map(PathBuf::from))
    {
        protected.push(config_dir);
    } else if let Ok(config_dir) = atomic_lib::config::default_config_dir_path() {
        protected.push(config_dir);
    }
    if let Some(cache_dir) = directory_override(args, "--cache-dir")
        .or_else(|| std::env::var_os("ATOMIC_CACHE_DIR").map(PathBuf::from))
        .or_else(|| project_dirs.as_ref().map(|dirs| dirs.cache_dir().to_path_buf()))
    {
        protected.push(cache_dir);
    }
    Ok(protected)
}

fn directory_override(args: &[OsString], name: &str) -> Option<PathBuf> {
    let inline = format!("{name}=");
    for (index, arg) in args.iter().enumerate() {
        let Some(arg) = arg.to_str() else {
            continue;
        };
        if arg == name {
            return args.get(index + 1).map(PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix(&inline) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

pub async fn restore(
    bundle_path: &Path,
    requested_target: &Path,
    protected: &[PathBuf],
) -> AtomicServerResult<()> {
    let bytes = read_bundle(bundle_path)?;
    let bundle = atomic_lib::recovery::decode_bundle(&bytes)?;
    let target = create_fresh_target(requested_target, protected)?;
    let uploads = target.join("uploads");
    if let Err(error) = fs::create_dir(&uploads) {
        return Err(format!(
            "Could not create isolated recovery uploads directory in {}: {error}",
            target.display()
        )
        .into());
    }

    let store = match atomic_lib::Db::init_redb_file(
        &target,
        bundle.manifest().base_domain.clone(),
        &uploads,
    )
    .await
    {
        Ok(store) => store,
        Err(error) => {
            return Err(format!(
                "Recovery target initialization failed; the newly owned directory is retained at {}: {error}",
                target.display()
            )
            .into());
        }
    };
    if let Err(error) = atomic_lib::recovery::restore_bundle(&store, &bundle).await {
        let _ = store.flush();
        return Err(format!(
            "Recovery restore failed; the isolated target is retained for inspection at {}: {error}",
            target.display()
        )
        .into());
    }
    store.flush()?;
    println!("Recovery restored and verified in {}", target.display());
    Ok(())
}

fn read_bundle(path: &Path) -> AtomicServerResult<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect recovery bundle {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Recovery bundle input must be a regular non-symlink file".into());
    }
    if metadata.len() > atomic_lib::recovery::MAX_BUNDLE_BYTES as u64 {
        return Err("Recovery bundle exceeds the 16 MiB limit".into());
    }
    let mut file = File::open(path)
        .map_err(|error| format!("Could not open recovery bundle {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(atomic_lib::recovery::MAX_BUNDLE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read recovery bundle {}: {error}", path.display()))?;
    if bytes.len() > atomic_lib::recovery::MAX_BUNDLE_BYTES {
        return Err("Recovery bundle exceeds the 16 MiB limit".into());
    }
    Ok(bytes)
}

fn create_fresh_target(target: &Path, protected: &[PathBuf]) -> AtomicServerResult<PathBuf> {
    if target.as_os_str().is_empty() {
        return Err("Recovery target directory must be explicit".into());
    }
    match fs::symlink_metadata(target) {
        Ok(_) => return Err("Recovery target must not already exist or be a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not inspect recovery target: {error}").into()),
    }

    let target = absolute_target(target)?;
    if has_active_store_ancestor(&target) {
        return Err("Recovery target is inside an existing active database directory".into());
    }
    for path in protected {
        let protected = resolve_path(path)?;
        if target.starts_with(&protected) || protected.starts_with(&target) {
            return Err(format!(
                "Recovery target overlaps a configured active or production path: {}",
                target.display()
            )
            .into());
        }
    }

    fs::create_dir(&target).map_err(|error| {
        format!(
            "Could not exclusively create recovery target {}: {error}",
            target.display()
        )
    })?;
    Ok(target)
}

fn absolute_target(path: &Path) -> AtomicServerResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Could not resolve current directory: {error}"))?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or("Recovery target must name a new directory")?;
    let parent = absolute
        .parent()
        .ok_or("Recovery target must have an existing parent directory")?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("Recovery target parent must already exist: {error}"))?;
    Ok(parent.join(file_name))
}

fn resolve_path(path: &Path) -> AtomicServerResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Could not resolve configured path: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    let mut probe = normalized.clone();
    let mut suffix = Vec::new();
    while !probe.exists() {
        let name = probe
            .file_name()
            .ok_or("Could not resolve configured protected path")?
            .to_os_string();
        suffix.push(name);
        if !probe.pop() {
            return Err("Could not resolve configured protected path".into());
        }
    }
    let mut resolved = fs::canonicalize(probe)
        .map_err(|error| format!("Could not resolve configured protected path: {error}"))?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn has_active_store_ancestor(target: &Path) -> bool {
    let mut ancestor = target.parent();
    while let Some(path) = ancestor {
        if path.join("atomic.redb").is_file()
            || (path.join("db").exists() && path.join("conf").exists())
        {
            return true;
        }
        ancestor = path.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn detects_recovery_after_global_value_options_without_opening_the_server_store() {
        let args = [
            "atomic-server",
            "--server-url",
            "http://localhost:9883",
            "recovery",
            "restore",
        ]
        .map(OsString::from);
        assert!(has_recovery_subcommand(&args));
    }

    #[test]
    fn does_not_mistake_a_directory_value_for_the_recovery_command() {
        let args = ["atomic-server", "--data-dir", "recovery"].map(OsString::from);
        assert!(!has_recovery_subcommand(&args));
    }

    #[test]
    fn fresh_target_is_exclusively_created_outside_protected_paths() {
        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("active-store");
        fs::create_dir(&protected).unwrap();
        let target = temp.path().join("restore-target");
        let created = create_fresh_target(&target, std::slice::from_ref(&protected)).unwrap();
        assert_eq!(created, target);
        assert!(created.is_dir());
        assert!(create_fresh_target(&target, &[protected]).is_err());
    }

    #[test]
    fn refuses_symlink_targets() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(create_fresh_target(&link, &[]).is_err());
    }

    #[test]
    fn refuses_active_store_and_ancestor_targets() {
        let temp = tempfile::tempdir().unwrap();
        let active_root = temp.path().join("production");
        let active_store = active_root.join("store");
        fs::create_dir_all(&active_store).unwrap();
        assert!(create_fresh_target(&active_store, std::slice::from_ref(&active_store)).is_err());
        assert!(create_fresh_target(&active_root, std::slice::from_ref(&active_store)).is_err());
        assert!(create_fresh_target(
            &active_store.join("recovery"),
            std::slice::from_ref(&active_store)
        )
        .is_err());
    }

    #[test]
    fn refuses_target_inside_database_even_without_configured_path() {
        let temp = tempfile::tempdir().unwrap();
        let active_store = temp.path().join("unconfigured-store");
        fs::create_dir(&active_store).unwrap();
        File::create(active_store.join("atomic.redb")).unwrap();
        assert!(create_fresh_target(&active_store.join("restore"), &[]).is_err());
    }

    #[actix_rt::test]
    async fn cli_restore_dispatches_to_a_fresh_native_target() {
        use atomic_lib::{agents::ForAgent, Storelike};

        let temp = tempfile::tempdir().unwrap();
        let store = atomic_lib::Db::init_temp(&format!(
            "recovery_cli_{}",
            atomic_lib::utils::random_string(10)
        ))
        .await
        .unwrap();
        store.set_envelope_retention(atomic_lib::envelopes::EnvelopeRetention::All);
        let owner = atomic_lib::agents::Agent::new(Some("CLI recovery owner")).unwrap();
        store.set_default_agent(owner.clone());
        let drive = store.ensure_private_drive().await.unwrap();
        store.set_active_drive(&drive).unwrap();
        let subject = store
            .create_resource(
                atomic_lib::urls::CLASS,
                &drive,
                "CLI recovery source",
                Some(vec![
                    (
                        atomic_lib::urls::DESCRIPTION,
                        atomic_lib::Value::Markdown("offline source bytes".into()),
                    ),
                    (
                        atomic_lib::urls::SHORTNAME,
                        atomic_lib::Value::Slug("recovery-cli-source".into()),
                    ),
                ]),
            )
            .await
            .unwrap();
        let base_domain = store.get_base_domain();
        let revision = |subject: &str| {
            store
                .get_resource_shallow(&atomic_lib::Subject::from_raw(
                    subject,
                    base_domain.as_deref(),
                ))
                .unwrap()
                .get(atomic_lib::urls::LAST_COMMIT)
                .unwrap()
                .to_string()
        };
        let request = atomic_lib::recovery::RecoveryRequest {
            version: 1,
            resources: vec![
                atomic_lib::recovery::RecoverySelection {
                    subject: drive.clone(),
                    expected_last_commit: revision(&drive),
                },
                atomic_lib::recovery::RecoverySelection {
                    subject: subject.clone(),
                    expected_last_commit: revision(&subject),
                },
            ],
            allowed_drives: vec![drive],
        };
        let bundle = atomic_lib::recovery::export_bundle(
            &store,
            &request,
            &ForAgent::AgentSubject(owner.subject),
        )
        .await
        .unwrap();
        let bundle_path = temp.path().join("recovery.bundle");
        let target = temp.path().join("fresh-target");
        fs::write(&bundle_path, bundle).unwrap();
        let args = vec![
            OsString::from("atomic-server"),
            OsString::from("recovery"),
            OsString::from("restore"),
            OsString::from("--bundle"),
            bundle_path.into_os_string(),
            OsString::from("--target-dir"),
            target.clone().into_os_string(),
        ];
        run_from_args(&args).await.unwrap();

        let uploads = target.join("uploads");
        let restored = atomic_lib::Db::init_redb_file(&target, base_domain, &uploads)
            .await
            .unwrap();
        let resource = restored
            .get_resource(&atomic_lib::Subject::from_raw(
                &subject,
                restored.get_base_domain().as_deref(),
            ))
            .await
            .unwrap();
        let description = match resource.get(atomic_lib::urls::DESCRIPTION).unwrap() {
            atomic_lib::Value::Markdown(text) | atomic_lib::Value::String(text) => text,
            value => panic!("unexpected restored description: {value:?}"),
        };
        assert_eq!(description, "offline source bytes");
    }
}
