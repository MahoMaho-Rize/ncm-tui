use std::{env, path::PathBuf, process::ExitCode};

use ncm_tui::library::Library;

fn main() -> ExitCode {
    let roots: Vec<PathBuf> = env::args_os().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        eprintln!("用法: cargo run --example import_music -- <音乐目录> [...]");
        return ExitCode::FAILURE;
    }

    match import(roots) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("导入失败: {error}");
            ExitCode::FAILURE
        }
    }
}

fn import(roots: Vec<PathBuf>) -> ncm_tui::library::Result<()> {
    let library = Library::open("./downloads")?;
    let report = library.scan(&roots)?;
    let stats = library.stats()?;
    println!(
        "发现 {}，新增 {}，更新 {}，未变化 {}，缺失 {}；音乐库现有 {} 首，{} 张专辑，总时长 {:.1} 小时",
        report.discovered,
        report.added,
        report.updated,
        report.unchanged,
        report.missing,
        stats.tracks,
        stats.albums,
        stats.duration_ms as f64 / 3_600_000.0
    );
    Ok(())
}
