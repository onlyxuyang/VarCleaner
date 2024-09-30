use glob::glob;
use glob::GlobError;
use glob::Pattern;
use path_slash::PathExt;
use std::collections::HashMap;
use std::collections::LinkedList;
use std::env;
use std::fs;
use std::fs::create_dir_all;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use threadpool::ThreadPool;
use threadpool_scope::scope_with;
use walkdir::{DirEntry, WalkDir};
use zip::result::ZipError;
use zip::write::SimpleFileOptions;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use winapi::um::winuser::{MessageBoxW, MB_OK, MB_SYSTEMMODAL};

fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn show_message_box(title: &str, message: &str) {
    let title_wide = to_wide_string(title);
    let message_wide = to_wide_string(message);

    unsafe {
        MessageBoxW(
            null_mut(),
            message_wide.as_ptr(),
            title_wide.as_ptr(),
            MB_OK | MB_SYSTEMMODAL,
        );
    }
}

fn file_op(is_copy: bool, src: &PathBuf, dst: &PathBuf) {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    if is_copy {
        let mut file = fs::File::open(src).unwrap();
        let mut outfile = fs::File::create(&dst).unwrap();
        io::copy(&mut file, &mut outfile).unwrap();
    } else {
        fs::rename(src, dst).unwrap();
    }
}

fn generate_duplicate_var_files(
    src_folder: &String,
) -> Result<HashMap<String, LinkedList<PathBuf>>, GlobError> {
    let mut result: HashMap<String, LinkedList<PathBuf>> = HashMap::new();
    for entry in glob(format!("{}/**/*.var", Pattern::escape(src_folder)).as_str())
        .expect("Failed to read glob pattern")
    {
        match entry {
            Ok(path) => {
                if !path.is_file() {
                    continue;
                }
                let filename = String::from(path.file_name().unwrap().to_str().unwrap());
                if !result.contains_key(&filename) {
                    result.insert(filename.clone(), LinkedList::new());
                }
                result.get_mut(&filename).unwrap().push_back(path);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(result)
}

fn get_short_path(path: &PathBuf, base: &PathBuf) -> PathBuf {
    return PathBuf::from_iter(path.strip_prefix(base).unwrap().components().skip(1));
}

fn zip_dir<T>(
    it: &mut dyn Iterator<Item = DirEntry>,
    prefix: &Path,
    writer: T,
    method: zip::CompressionMethod,
) -> anyhow::Result<()>
where
    T: Write + Seek,
{
    let mut zip = zip::ZipWriter::new(writer);
    let options = SimpleFileOptions::default()
        .compression_method(method)
        .unix_permissions(0o755)
        .with_alignment(4096);

    let prefix = Path::new(prefix);
    let mut buffer = Vec::new();
    for entry in it {
        let path = entry.path();
        let name = path.strip_prefix(prefix).unwrap();
        let path_as_string = name.to_slash().unwrap();

        // Write file or directory explicitly
        // Some unzip tools unzip files with directory paths correctly, some do not!
        if path.is_file() {
            //println!("adding file {path:?} as {name:?} ...");
            zip.start_file(path_as_string, options)?;
            let mut f = File::open(path)?;

            f.read_to_end(&mut buffer)?;
            zip.write_all(&buffer)?;
            buffer.clear();
        } else if !name.as_os_str().is_empty() {
            // Only if not root! Avoids path spec / warning
            // and mapname conversion failed error on unzip
            //println!("adding dir {path_as_string:?} as {name:?} ...");
            zip.add_directory(path_as_string, options)?;
        }
    }
    zip.finish()?;
    Ok(())
}

fn zip_one_file(
    src_dir: &Path,
    dst_file: &Path,
    method: zip::CompressionMethod,
) -> anyhow::Result<()> {
    if !Path::new(src_dir).is_dir() {
        println!(
            "Path {} is not directory, error.",
            src_dir.as_os_str().to_str().unwrap()
        );
        return Err(ZipError::FileNotFound.into());
    }
    fs::create_dir_all(dst_file.parent().unwrap()).unwrap();

    let path = Path::new(dst_file);
    let file = File::create(path).unwrap();

    let walkdir = WalkDir::new(src_dir);
    let it = walkdir.into_iter();

    zip_dir(&mut it.filter_map(|e| e.ok()), src_dir, file, method)?;
    Ok(())
}

fn rezip_one_file(src: &PathBuf, target: &PathBuf) {
    let mut result: HashMap<String, (PathBuf, u64)> = HashMap::new();
    let pattern = format!(
        "{}/**/*",
        Pattern::escape(src.as_os_str().to_str().unwrap())
    );
    for entry in glob(&pattern).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => {
                if !path.is_file() {
                    continue;
                }
                let short_name = get_short_path(&path, src);
                let short_name_str = short_name.as_os_str().to_str().unwrap().to_string();
                if !result.contains_key(short_name.as_os_str().to_str().unwrap()) {
                    result.insert(
                        short_name_str,
                        (path.clone(), fs::metadata(&path).unwrap().file_size()),
                    );
                } else {
                    let size = fs::metadata(&path).unwrap().file_size();
                    if result.get(&short_name_str).unwrap().1 < size {
                        *result.get_mut(&short_name_str).unwrap() = (path.clone(), size);
                    }
                }
            }
            Err(_) => panic!(),
        }
    }
    // If all duplicated var files are invalid, no file can be compress, just leave it
    if result.len() == 0 {
        return;
    }

    let workdir = src.join("working");
    for (short_name, (path, _)) in result.iter() {
        let filepath = workdir.join(short_name);
        file_op(false, path, &filepath);
    }
    zip_one_file(&workdir, target, zip::CompressionMethod::Stored).unwrap();
}

fn unzip_one_file(path: &PathBuf, base: &PathBuf, idx: usize) {
    let mut archive = match zip::ZipArchive::new(
        fs::File::open(path)
            .expect(format!("Could not open file {}", path.as_os_str().to_str().unwrap()).as_str()),
    ) {
        Ok(ret) => ret,
        Err(_) => {
            println!("zipfile {} is invaild", path.as_os_str().to_str().unwrap());
            return;
        }
    };

    for i in 0..archive.len() {
        let mut file = match archive.by_index(i) {
            Ok(tfile) => tfile,
            Err(_) => {
                println!("file error, ignore");
                continue;
            }
        };
        let outpath = match file.enclosed_name() {
            Some(path) => path,
            None => continue,
        };

        let realoutpath = base.join(idx.to_string()).join(outpath);

        if file.is_dir() {
            fs::create_dir_all(&realoutpath).unwrap();
        } else {
            if let Some(p) = realoutpath.parent() {
                if !p.exists() {
                    fs::create_dir_all(p).unwrap();
                }
            }
            let mut outfile = fs::File::create(&realoutpath).unwrap();
            io::copy(&mut file, &mut outfile).unwrap();
        }
    }
}

fn main() {
    if !fs::exists("VaM.exe").unwrap() {
        println!("Please put VarCleaner.exe under VaM folder which includes VaM.exe \n 请将VarCleaner.exe放在VaM.exe同级目录下");
        show_message_box("Error/错误", "Please put VarCleaner.exe under VaM folder which includes VaM.exe \n 请将VarCleaner.exe放在VaM.exe同级目录下");
        return;
    }
    let vam_folder = env::current_dir().unwrap();
    let var_folder = &vam_folder.join("AddonPackages");
    let var_merged_folder = &PathBuf::from(&var_folder).join("merged");
    let var_backup_folder = &PathBuf::from(&vam_folder).join("VarCleaner/Backup");
    let dst_tmp_folder = &PathBuf::from(&vam_folder).join("VarCleaner/Tmp");
    let var_folder_str = var_folder.to_string_lossy();
    let var_merged_folder_str = var_merged_folder.to_string_lossy();
    let var_backup_folder_str = var_backup_folder.to_string_lossy();
    println!("VarCleaner will put merged duplicated var to {var_merged_folder_str}, and backup original var at {var_backup_folder_str}");
    println!("VarCleaner 将清理过的重复Var放在{var_merged_folder_str}, 并将原始Var备份在{var_backup_folder_str}");

    let hpool = ThreadPool::new(12);
    let file_dicts = generate_duplicate_var_files(&var_folder_str.to_string()).unwrap();
    scope_with(&hpool, |hscope| {
        for (filename, filelist) in file_dicts.iter() {
            let filename_clone = filename.clone();
            let filelist_clone = filelist.clone();
            hscope.execute(move || {
                if filelist_clone.len() > 1 {
                    println!(
                        "Process file {} Count {}",
                        filename_clone,
                        filelist_clone.len()
                    );
                    let pool = ThreadPool::new(filelist_clone.len());
                    let var_tmp_folder = &dst_tmp_folder.join(PathBuf::from(&filename_clone));
                    scope_with(&pool, |scope| {
                        for (pos, item) in filelist_clone.iter().enumerate() {
                            let item_clone = item.clone();
                            scope.execute(move || {
                                let relative_path = item_clone.strip_prefix(var_folder).unwrap();
                                let backup_var_path = var_backup_folder.join(relative_path);
                                unzip_one_file(&item_clone, &var_tmp_folder, pos);
                                create_dir_all(backup_var_path.parent().unwrap()).unwrap();
                                file_op(false, &item_clone, &backup_var_path);
                            });
                        }
                    });
                    if fs::exists(var_tmp_folder).unwrap() {
                        rezip_one_file(&var_tmp_folder, &var_merged_folder.join(&filename_clone));
                        fs::remove_dir_all(&var_tmp_folder).unwrap();
                    }
                }
            });
        }
    });
    if fs::exists(&dst_tmp_folder).unwrap() {
        fs::remove_dir_all(&dst_tmp_folder).unwrap();
    }
    println!("Done/完成清理");
    show_message_box("Success/成功", "Done/完成清理");
}
