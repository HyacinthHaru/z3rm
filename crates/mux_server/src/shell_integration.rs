use anyhow::{Context as _, Result};
#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
use portable_pty::CommandBuilder;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const INSTALL_ROOT_ENV: &str = "Z3RM_SHELL_INTEGRATION_DIR";
const INSTALL_VERSION: &str = "v1";

static INSTALL_LOCK: Mutex<()> = Mutex::new(());

const ZSH_ENV: &str = r#"# z3rm shell integration
typeset _z3rm_original_zdotdir="$Z3RM_ZSH_USER_ZDOTDIR"
export ZDOTDIR="$_z3rm_original_zdotdir"
if [[ -r "$_z3rm_original_zdotdir/.zshenv" ]]; then
    source "$_z3rm_original_zdotdir/.zshenv"
fi
typeset -g Z3RM_ZSH_USER_ZDOTDIR="${ZDOTDIR:-$HOME}"
if [[ "$Z3RM_ZSH_USER_ZDOTDIR" == "$Z3RM_ZSH_INTEGRATION_DIR" ]]; then
    Z3RM_ZSH_USER_ZDOTDIR="$_z3rm_original_zdotdir"
fi
export ZDOTDIR="$Z3RM_ZSH_INTEGRATION_DIR"
unset _z3rm_original_zdotdir
"#;

const ZSH_RC: &str = r#"# z3rm shell integration
typeset _z3rm_user_zdotdir="$Z3RM_ZSH_USER_ZDOTDIR"
export ZDOTDIR="$_z3rm_user_zdotdir"
if [[ -r "$_z3rm_user_zdotdir/.zshrc" ]]; then
    source "$_z3rm_user_zdotdir/.zshrc"
fi
if [[ "$ZDOTDIR" == "$Z3RM_ZSH_INTEGRATION_DIR" ]]; then
    export ZDOTDIR="$_z3rm_user_zdotdir"
fi
unset _z3rm_user_zdotdir

typeset -gi _z3rm_command_active=0

_z3rm_precmd() {
    local command_status=$?
    if (( _z3rm_command_active )); then
        printf '\033]133;D;%d\007' "$command_status"
        _z3rm_command_active=0
    fi
    printf '\033]133;A\007'
    return "$command_status"
}

_z3rm_preexec() {
    local command_status=$?
    printf '\033]133;C\007'
    _z3rm_command_active=1
    return "$command_status"
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _z3rm_precmd
add-zsh-hook preexec _z3rm_preexec
precmd_functions=(_z3rm_precmd ${precmd_functions:#_z3rm_precmd})
preexec_functions=(_z3rm_preexec ${preexec_functions:#_z3rm_preexec})
PS1="${PS1}"$'%{\e]133;B\a%}'
"#;

const BASH_RC: &str = r#"# z3rm shell integration
_z3rm_bash_user_rc="$Z3RM_BASH_USER_RC"
if [[ -r "$_z3rm_bash_user_rc" ]]; then
    source "$_z3rm_bash_user_rc"
fi
unset _z3rm_bash_user_rc

_z3rm_existing_debug_trap="$(trap -p DEBUG)"
_z3rm_command_active=0
_z3rm_in_prompt_command=0
_z3rm_prompt_seen=0
_z3rm_ready_for_command=0

_z3rm_prompt_start() {
    local command_status=$?
    _z3rm_in_prompt_command=1
    if (( _z3rm_command_active )) || { [[ -n "$_z3rm_existing_debug_trap" ]] && (( _z3rm_prompt_seen )); }; then
        printf '\033]133;D;%d\007' "$command_status"
        _z3rm_command_active=0
    fi
    printf '\033]133;A\007'
    _z3rm_prompt_seen=1
    return "$command_status"
}

_z3rm_prompt_end() {
    local command_status=$?
    _z3rm_in_prompt_command=0
    _z3rm_ready_for_command=1
    return "$command_status"
}

_z3rm_debug_trap() {
    local command_status=$?
    local pending_command="$1"
    if (( _z3rm_in_prompt_command )) || [[ "$pending_command" == _z3rm_prompt_start* ]] || [[ "$pending_command" == _z3rm_prompt_end* ]]; then
        return "$command_status"
    fi
    if (( _z3rm_ready_for_command )); then
        printf '\033]133;C\007'
        _z3rm_command_active=1
        _z3rm_ready_for_command=0
    fi
    return "$command_status"
}

case "$(declare -p PROMPT_COMMAND 2>/dev/null)" in
    "declare -a"*) PROMPT_COMMAND=(_z3rm_prompt_start "${PROMPT_COMMAND[@]}" _z3rm_prompt_end) ;;
    *)
        if [[ -n "$PROMPT_COMMAND" ]]; then
            PROMPT_COMMAND="_z3rm_prompt_start; $PROMPT_COMMAND; _z3rm_prompt_end"
        else
            PROMPT_COMMAND="_z3rm_prompt_start; _z3rm_prompt_end"
        fi
        ;;
esac
PS1="${PS1}"'\[\e]133;B\a\]'

if [[ -n "$_z3rm_existing_debug_trap" ]]; then
    printf 'z3rm: existing DEBUG trap retained; command-start tracking is limited\n' >&2
else
    trap '_z3rm_debug_trap "$BASH_COMMAND"' DEBUG
fi
"#;

#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
pub(crate) fn default_shell_command(shell: &str) -> CommandBuilder {
    let mut command = CommandBuilder::new(shell);
    if !is_supported_shell(shell) {
        return command;
    }
    let result = install_root().and_then(|root| configure_shell(&mut command, shell, &root));
    if let Err(error) = result {
        zlog::warn!(
            "shell integration unavailable for {}: {:#}; starting the original shell",
            shell,
            error
        );
        return CommandBuilder::new(shell);
    }
    command
}

fn is_supported_shell(shell: &str) -> bool {
    Path::new(shell)
        .file_name()
        .is_some_and(|name| name == OsStr::new("zsh") || name == OsStr::new("bash"))
}

#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
fn install_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os(INSTALL_ROOT_ENV) {
        return Ok(PathBuf::from(root));
    }
    dirs::data_local_dir()
        .context("cannot determine the local data directory")
        .map(|directory| {
            directory
                .join("z3rm")
                .join("shell-integration")
                .join(INSTALL_VERSION)
        })
}

#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
fn configure_shell(command: &mut CommandBuilder, shell: &str, root: &Path) -> Result<()> {
    let Some(shell_name) = Path::new(shell).file_name() else {
        return Ok(());
    };
    if shell_name != OsStr::new("zsh") && shell_name != OsStr::new("bash") {
        return Ok(());
    }

    let _install_guard = INSTALL_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("shell integration install lock is poisoned"))?;
    secure_directory(root)?;

    if shell_name == OsStr::new("zsh") {
        configure_zsh(command, root)
    } else {
        configure_bash(command, root)
    }
}

#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
fn configure_zsh(command: &mut CommandBuilder, root: &Path) -> Result<()> {
    let zsh_directory = root.join("zsh");
    secure_directory(&zsh_directory)?;
    atomic_write(&zsh_directory.join(".zshenv"), ZSH_ENV.as_bytes())?;
    atomic_write(&zsh_directory.join(".zshrc"), ZSH_RC.as_bytes())?;

    let home = command
        .get_env("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set for zsh")?;
    let user_zdotdir = command
        .get_env("ZDOTDIR")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .unwrap_or(home);
    if user_zdotdir == zsh_directory {
        anyhow::bail!("the user's ZDOTDIR points at the managed integration directory");
    }

    command.env("Z3RM_ZSH_USER_ZDOTDIR", user_zdotdir);
    command.env("Z3RM_ZSH_INTEGRATION_DIR", &zsh_directory);
    command.env("ZDOTDIR", zsh_directory);
    Ok(())
}

#[cfg(all(
    not(target_family = "wasm"),
    any(feature = "desktop", feature = "guest")
))]
fn configure_bash(command: &mut CommandBuilder, root: &Path) -> Result<()> {
    let bash_rc = root.join("bashrc");
    atomic_write(&bash_rc, BASH_RC.as_bytes())?;

    let home = command
        .get_env("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set for bash")?;
    command.env("Z3RM_BASH_USER_RC", home.join(".bashrc"));
    command.arg("--rcfile");
    command.arg(bash_rc);
    Ok(())
}

fn secure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => verify_directory(path, &metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("create managed directory {}", path.display()))?;
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("inspect managed directory {}", path.display()))?;
            verify_directory(path, &metadata)?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect managed directory {}", path.display()));
        }
    }

    set_directory_permissions(path)?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("verify managed directory {}", path.display()))?;
    verify_directory(path, &metadata)
}

fn verify_directory(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("managed path {} is not a real directory", path.display());
    }
    verify_owner(path, metadata)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("managed path {} is not a regular file", path.display());
            }
            verify_owner(path, &metadata)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect managed file {}", path.display()));
        }
    }

    let parent = path
        .parent()
        .with_context(|| format!("managed file {} has no parent", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("managed shell integration file name is not valid UTF-8")?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", nanoid::nanoid!()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_open_permissions(&mut options);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create temporary file {}", temporary.display()))?;
    let write_result = (|| -> Result<()> {
        file.write_all(contents)
            .with_context(|| format!("write temporary file {}", temporary.display()))?;
        set_file_permissions(&temporary)?;
        file.sync_all()
            .with_context(|| format!("sync temporary file {}", temporary.display()))?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        return cleanup_temporary(&temporary, error);
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return cleanup_temporary(
                    &temporary,
                    anyhow::anyhow!("managed path {} was replaced", path.display()),
                );
            }
            if let Err(error) = verify_owner(path, &metadata) {
                return cleanup_temporary(&temporary, error);
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return cleanup_temporary(
                &temporary,
                anyhow::Error::from(error)
                    .context(format!("inspect managed file {}", path.display())),
            );
        }
    }
    if let Err(error) = fs::rename(&temporary, path) {
        return cleanup_temporary(
            &temporary,
            anyhow::Error::from(error).context(format!("install managed file {}", path.display())),
        );
    }

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("verify managed file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("installed path {} is not a regular file", path.display());
    }
    verify_owner(path, &metadata)?;
    verify_file_permissions(path, &metadata)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync managed directory {}", parent.display()))?;
    Ok(())
}

fn cleanup_temporary(path: &Path, error: anyhow::Error) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Err(error),
        Err(cleanup_error) if cleanup_error.kind() == ErrorKind::NotFound => Err(error),
        Err(cleanup_error) => Err(error.context(format!(
            "also failed to remove temporary file {}: {}",
            path.display(),
            cleanup_error
        ))),
    }
}

#[cfg(unix)]
fn verify_owner(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("managed path {} is owned by another user", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure managed directory {}", path.display()))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_open_permissions(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_open_permissions(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure managed file {}", path.display()))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_file_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode() & 0o777 != 0o600 {
        anyhow::bail!("managed file {} does not have mode 0600", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_file_permissions(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_uses_managed_zdotdir_without_changing_user_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().join("home");
        let user_zdotdir = temporary.path().join("user-zdotdir");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&user_zdotdir)?;
        let user_rc = user_zdotdir.join(".zshrc");
        fs::write(&user_rc, "user rc\n")?;
        let root = temporary.path().join("integration");
        let mut command = CommandBuilder::new("/bin/zsh");
        command.env("HOME", &home);
        command.env("ZDOTDIR", &user_zdotdir);

        configure_shell(&mut command, "/bin/zsh", &root)?;

        assert_eq!(
            command.get_env("ZDOTDIR"),
            Some(root.join("zsh").as_os_str())
        );
        assert_eq!(
            command.get_env("Z3RM_ZSH_USER_ZDOTDIR"),
            Some(user_zdotdir.as_os_str())
        );
        assert_eq!(fs::read_to_string(&user_rc)?, "user rc\n");
        assert!(fs::read_to_string(root.join("zsh/.zshrc"))?.contains("%{\\e]133;B\\a%}"));
        assert_eq!(command.get_argv().len(), 1);
        Ok(())
    }

    #[test]
    fn bash_uses_managed_rcfile_without_changing_user_bashrc() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().join("home");
        fs::create_dir_all(&home)?;
        let user_rc = home.join(".bashrc");
        fs::write(&user_rc, "user rc\n")?;
        let root = temporary.path().join("integration");
        let mut command = CommandBuilder::new("/bin/bash");
        command.env("HOME", &home);

        configure_shell(&mut command, "/bin/bash", &root)?;

        assert_eq!(fs::read_to_string(&user_rc)?, "user rc\n");
        assert_eq!(
            command.get_env("Z3RM_BASH_USER_RC"),
            Some(user_rc.as_os_str())
        );
        assert_eq!(command.get_argv()[1], OsStr::new("--rcfile"));
        assert_eq!(command.get_argv()[2], root.join("bashrc").as_os_str());
        assert!(fs::read_to_string(root.join("bashrc"))?.contains("\\[\\e]133;B\\a\\]"));
        Ok(())
    }

    #[test]
    fn unsupported_shell_does_not_install_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("integration");
        let mut command = CommandBuilder::new("/bin/sh");

        configure_shell(&mut command, "/bin/sh", &root)?;

        assert!(!root.exists());
        assert_eq!(command.get_argv().len(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_managed_file_symlinks_and_keeps_the_target_unchanged() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("integration");
        secure_directory(&root)?;
        let target = temporary.path().join("target");
        fs::write(&target, "do not replace\n")?;
        symlink(&target, root.join("bashrc"))?;
        let mut command = CommandBuilder::new("/bin/bash");
        command.env("HOME", temporary.path());

        let error = configure_shell(&mut command, "/bin/bash", &root)
            .expect_err("a managed file symlink must be rejected");

        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
        );
        assert_eq!(fs::read_to_string(target)?, "do not replace\n");
        assert_eq!(fs::metadata(&root)?.permissions().mode() & 0o777, 0o700);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn installs_private_files() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("integration");
        let mut command = CommandBuilder::new("/bin/bash");
        command.env("HOME", temporary.path());

        configure_shell(&mut command, "/bin/bash", &root)?;

        assert_eq!(fs::metadata(&root)?.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(root.join("bashrc"))?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }
}
