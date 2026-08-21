use std::path::PathBuf;

/// Prefer Git for Windows over the WSL compatibility shim on Windows.
pub fn bash_executable() -> PathBuf {
    #[cfg(windows)]
    for variable in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable) {
            let candidate = PathBuf::from(root).join("Git").join("bin").join("bash.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from("bash")
}
