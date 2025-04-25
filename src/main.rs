use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use image::{ImageBuffer, ImageFormat, Rgba};
use log::{LevelFilter, error, info};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use psd::Psd;
use walkdir::WalkDir;

// 定义防抖间隔，这里是 100 毫秒 (0.1 秒)
const DEBOUNCE_DURATION: Duration = Duration::from_millis(100);

// 定义支持的导出格式
#[derive(ValueEnum, Clone, Debug)] // 派生 ValueEnum, Clone, Debug
enum ExportFormat {
    Png,
    Jpg,
}

impl ExportFormat {
    // 获取对应的文件扩展名列表
    fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Png => "png",
            ExportFormat::Jpg => "jpg",
        }
    }

    // 获取对应的 image crate 输出格式
    fn image_format(&self) -> ImageFormat {
        match self {
            ExportFormat::Png => ImageFormat::Png,
            ExportFormat::Jpg => ImageFormat::Jpeg,
        }
    }
}

/// 监听指定路径下的 PSD 文件变化（支持文件夹递归或单个文件）并自动导出为
/// PNG/JPG
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// 要监听的文件夹路径（递归监听）或单个 PSD 文件路径
    path: PathBuf,

    /// 导出图像的格式 (png 或 jpg)
    #[arg(short, long, value_enum, default_value_t = ExportFormat::Png)]
    format: ExportFormat,

    /// 只导出一次现有的 PSD 文件，不持续监听
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    _ = pretty_env_logger::formatted_builder()
        .filter_level(LevelFilter::Info)
        .format_timestamp_secs()
        .parse_default_env()
        .try_init();

    // 解析命令行参数
    let args = Cli::parse();
    let watch_path = args.path;
    let export_format = args.format;
    let run_once = args.once;

    // 检查监听路径是否存在
    if !watch_path.exists() {
        error!("错误：指定的路径不存在：{:?}", watch_path);
        std::process::exit(1);
    }

    // 如果是一次性模式
    if run_once {
        info!("以一次性模式运行，导出现有文件...");
        let psd_files = find_psd_files(&watch_path)?;
        info!("找到 {} 个 .psd 文件。", psd_files.len());

        let mut handles = vec![];

        if psd_files.is_empty() {
            info!("没有找到需要导出的 .psd 文件。");
        } else {
            for psd_path in psd_files {
                info!("正在安排导出文件：{:?}", psd_path);
                let psd_path_clone = psd_path.clone();
                let export_format_clone = export_format.clone(); // 克隆格式参数
                let handle = thread::spawn(move || {
                    info!("正在导出文件：{:?}", psd_path_clone);
                    match process_psd_file(&psd_path_clone, &export_format_clone) {
                        Ok(_) => info!(
                            "成功导出：{:?} -> {:?}",
                            psd_path_clone,
                            psd_path_clone.with_extension(export_format_clone.extension())
                        ),
                        Err(e) => error!("导出文件失败 {:?}: {}", psd_path_clone, e),
                    }
                });
                handles.push(handle);
            }

            // 等待所有处理线程完成
            for handle in handles {
                handle.join().expect("处理线程崩溃");
            }
            info!("一次性导出完成。");
        }

        Ok(()) // 一次性模式完成后退出
    } else {
        // 持续监听模式

        // 根据路径类型确定监听模式
        let recursive_mode = if watch_path.is_dir() {
            info!("开始递归监听目录：{:?}", watch_path);
            RecursiveMode::Recursive
        } else if watch_path.is_file() {
            // 如果是文件，检查是否是 .psd 文件
            if watch_path.extension().and_then(|ext| ext.to_str()) != Some("psd") {
                error!(
                    "错误：指定的路径是一个文件，但不是 .psd 文件：{:?}",
                    watch_path
                );
                std::process::exit(1);
            }
            info!("开始监听单个文件：{:?}", watch_path);
            RecursiveMode::NonRecursive // 监听单个文件不需要递归
        } else {
            // 既不是文件也不是目录，报错退出
            error!("错误：指定的路径既不是文件也不是目录：{:?}", watch_path);
            std::process::exit(1);
        };

        // 创建一个通道用于接收文件系统事件
        let (tx, rx) = mpsc::channel();

        // 创建一个文件系统监听器
        let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())
            .context("无法创建文件系统监听器")?;

        // 开始监听指定的路径，根据类型使用不同的模式
        watcher
            .watch(&watch_path, recursive_mode)
            .context(format!("无法监听路径：{:?}", watch_path))?;

        info!("监听器已启动。等待 .psd 文件创建或修改...");
        info!("导出格式：{:?}", export_format);
        info!("防抖间隔设置为：{:?}", DEBOUNCE_DURATION);

        // 使用 Arc<Mutex<HashMap>>
        // 来存储每个文件上次导出的时间，以便在多个线程间安全共享
        let last_processed_times: Arc<Mutex<HashMap<PathBuf, Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // 在主线程中导出接收到的事件
        for res in rx {
            match res {
                Ok(event) => {
                    // 只处理创建和修改事件
                    if let EventKind::Create(_) | EventKind::Modify(_) = event.kind {
                        // 遍历事件中涉及的所有路径
                        for path in event.paths {
                            // 检查路径是否是文件且以 .psd 结尾
                            if path.is_file()
                                && path.extension().and_then(|ext| ext.to_str()) == Some("psd")
                            {
                                // 获取当前时间
                                let now = Instant::now();

                                // 获取互斥锁，访问 last_processed_times map
                                let mut map = last_processed_times.lock().unwrap();

                                // 检查该文件上次导出的时间
                                if let Some(last_time) = map.get(&path) {
                                    // 如果距离上次导出时间小于防抖间隔，则忽略此事件
                                    if now.duration_since(*last_time) < DEBOUNCE_DURATION {
                                        info!("文件 {:?} 在防抖间隔内，忽略事件。", path);
                                        continue; // 跳过当前路径的导出
                                    }
                                }

                                // 如果是第一次导出，或者距离上次导出时间已超过防抖间隔
                                info!("检测到 .psd 文件事件：{:?}", path);

                                // 更新该文件的导出时间
                                map.insert(path.clone(), now);

                                // 释放互斥锁，避免在导出过程中阻塞其他事件的导出
                                drop(map);

                                // 克隆路径和格式参数，因为新线程需要拥有它们
                                let psd_path_clone = path.clone();
                                let export_format_clone = export_format.clone();

                                // 在新线程中处理 PSD 到 PNG 的转换
                                thread::spawn(move || {
                                    std::thread::sleep(Duration::from_millis(10)); // 避免 psd 还未写入就开始读取，然后失败。
                                    info!("正在导出文件：{:?}", psd_path_clone);
                                    match process_psd_file(&psd_path_clone, &export_format_clone) {
                                        Ok(_) => info!(
                                            "成功导出：{:?} -> {:?}",
                                            psd_path_clone,
                                            psd_path_clone
                                                .with_extension(export_format_clone.extension())
                                        ),
                                        Err(e) => {
                                            error!("导出文件失败 {:?}: {}", psd_path_clone, e)
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
                Err(e) => error!("监听事件错误：{}", e),
            }
        }

        // 如果 rx 循环结束（通常不会发生，除非监听器停止），程序退出
        info!("监听器停止。");

        Ok(())
    }
}

/// 查找指定路径下的所有 .psd 文件（如果是目录则递归查找）
fn find_psd_files(path: &Path) -> Result<Vec<PathBuf>> {
    let mut psd_files = Vec::new();

    if path.is_file() {
        if path.extension().and_then(|ext| ext.to_str()) == Some("psd") {
            psd_files.push(path.to_path_buf());
        }
    } else if path.is_dir() {
        for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            let entry_path = entry.path();
            if entry_path.is_file()
                && entry_path.extension().and_then(|ext| ext.to_str()) == Some("psd")
            {
                psd_files.push(entry_path.to_path_buf());
            }
        }
    }
    // 如果路径不存在或不是文件/目录，find_psd_files 会返回空 Vec，这在 main
    // 中已经处理了路径不存在的情况

    Ok(psd_files)
}

/// 将指定的 PSD 文件转换为同名的指定格式图像文件
fn process_psd_file(psd_path: &Path, format: &ExportFormat) -> Result<()> {
    // 构建输出文件的路径，使用指定的扩展名
    let output_path = psd_path.with_extension(format.extension());

    // 读取 PSD 文件内容
    let psd_bytes =
        std::fs::read(psd_path).context(format!("无法读取 PSD 文件：{:?}", psd_path))?;

    // 解析 PSD 数据
    let psd = Psd::from_bytes(&psd_bytes).context(format!("无法解析 PSD 文件：{:?}", psd_path))?;

    // 获取合并后的最终图像数据 (RGBA 格式)
    let final_image_data: Vec<u8> = psd.rgba();

    // 创建 ImageBuffer
    let img_buffer =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(psd.width(), psd.height(), final_image_data)
            .context("无法创建 ImageBuffer，可能是图像数据或尺寸问题")?;

    // 保存为指定格式的图像文件
    // image crate 的 save 方法可以根据文件扩展名自动选择格式，
    // 但为了明确控制格式（特别是 JPEG 质量），我们使用 write_to
    let mut file = std::fs::File::create(&output_path)
        .context(format!("无法创建输出文件：{:?}", output_path))?;

    img_buffer
        .write_to(&mut file, format.image_format())
        .context(format!("无法保存图像文件：{:?}", output_path))?;

    Ok(())
}
