//! 应用数据目录与路径（唯一路径来源）。
//!
//! 根目录：`%LOCALAPPDATA%\桌面旁白记录器`
//! - `data/`   记录 `YYYY-MM-DD.md`
//! - `output/` 旁白 `YYYY-MM-DD.md`
//! - `logs/`   日志 `app-YYYY-MM-DD.log`
//! - `config.json` 配置（apikey/baseURL/model）
//!
//! `LOCALAPPDATA` 缺失时回退 `APPDATA`，再缺失回退当前目录，并记录 warning。
//! 业务代码不得硬编码任何路径，一律通过 `paths::get()`。

use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

/// 应用数据根目录名（用户已确认）。
pub const ROOT_DIR_NAME: &str = "桌面旁白记录器";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub data_dir: PathBuf,
    pub output_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub config_file: PathBuf,
}

impl AppPaths {
    /// 由根目录推导出全部路径（供测试与 `resolve` 共用）。
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            data_dir: root.join("data"),
            output_dir: root.join("output"),
            logs_dir: root.join("logs"),
            config_file: root.join("config.json"),
            root,
        }
    }

    /// 解析真实根目录；返回路径与需要写日志的 warning。
    pub fn resolve() -> (Self, Vec<String>) {
        let mut warnings: Vec<String> = Vec::new();

        let base = match env::var_os("LOCALAPPDATA") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => {
                warnings.push("LOCALAPPDATA 缺失，回退到 APPDATA".to_string());
                match env::var_os("APPDATA") {
                    Some(value) if !value.is_empty() => PathBuf::from(value),
                    _ => {
                        warnings.push(
                            "APPDATA 也缺失，回退到当前工作目录（数据可能不稳定）".to_string(),
                        );
                        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    }
                }
            }
        };

        (Self::from_root(base.join(ROOT_DIR_NAME)), warnings)
    }

    /// 启动时确保根目录与三个子目录存在；失败只返回错误描述，不崩溃。
    pub fn ensure_dirs(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for dir in [&self.root, &self.data_dir, &self.output_dir, &self.logs_dir] {
            if let Err(error) = std::fs::create_dir_all(dir) {
                errors.push(format!("创建目录失败 {}: {error}", dir.display()));
            }
        }
        errors
    }

    /// 记录文件：`data/YYYY-MM-DD.md`（文件名用「这天的日期」）。
    pub fn data_file(&self, day_key: &str) -> PathBuf {
        self.data_dir.join(format!("{day_key}.md"))
    }

    /// 旁白输出：`output/YYYY-MM-DD.md`（文件名同记录日期）。
    pub fn output_file(&self, day_key: &str) -> PathBuf {
        self.output_dir.join(format!("{day_key}.md"))
    }
}

static PATHS: OnceLock<AppPaths> = OnceLock::new();
static PATHS_WARNINGS: OnceLock<Vec<String>> = OnceLock::new();

/// 初始化（幂等）：解析路径、创建目录，并缓存 warning。
pub fn init() -> &'static AppPaths {
    PATHS.get_or_init(|| {
        let (paths, mut warnings) = AppPaths::resolve();
        warnings.extend(paths.ensure_dirs());
        let _ = PATHS_WARNINGS.set(warnings);
        paths
    })
}

/// 全局唯一路径入口；未显式 init 时也会惰性初始化。
pub fn get() -> &'static AppPaths {
    init()
}

/// 取出初始化期间累积的 warning（供启动时写日志）。
pub fn init_warnings() -> Vec<String> {
    let _ = init();
    PATHS_WARNINGS.get().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn from_root_derives_known_paths() {
        let paths = AppPaths::from_root(PathBuf::from(r"C:\Users\demo\AppData\Local\桌面旁白记录器"));
        assert!(paths.data_dir.ends_with("data"));
        assert!(paths.output_dir.ends_with("output"));
        assert!(paths.logs_dir.ends_with("logs"));
        assert!(paths.config_file.ends_with("config.json"));
        assert_eq!(paths.root.file_name().unwrap(), ROOT_DIR_NAME);
    }

    #[test]
    fn data_and_output_files_follow_contract() {
        let paths = AppPaths::from_root(PathBuf::from(r"C:\tmp\app"));
        assert_eq!(
            paths.data_file("2026-11-15"),
            PathBuf::from(r"C:\tmp\app\data\2026-11-15.md")
        );
        assert_eq!(
            paths.output_file("2026-11-15"),
            PathBuf::from(r"C:\tmp\app\output\2026-11-15.md")
        );
    }
}
