use crate::domain::file::{FileItem, FileInfo};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use chrono::{DateTime, Utc};
use tauri_plugin_dialog::DialogExt;

#[derive(Clone)]
pub struct FileState;

impl FileState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn list_files(path: String) -> Result<Vec<FileItem>, String> {
    let path = PathBuf::from(&path);

    if !path.exists() {
        return Err(format!("Path does not exist: {}", path.display()));
    }

    let mut items = Vec::new();

    if path.is_dir() {
        let entries = fs::read_dir(&path)
            .map_err(|e| format!("Failed to read directory: {}", e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
            let metadata = entry.metadata().map_err(|e| format!("Failed to read metadata: {}", e))?;

            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0));

            items.push(FileItem {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                is_dir: metadata.is_dir(),
                size: if metadata.is_file() { Some(metadata.len()) } else { None },
                modified,
            });
        }

        items.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                b.is_dir.cmp(&a.is_dir)
            } else {
                a.name.cmp(&b.name)
            }
        });
    }

    Ok(items)
}

#[tauri::command]
pub async fn read_file(path: String) -> Result<String, String> {
    fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read file: {}", e))
}

#[tauri::command]
pub async fn write_file(path: String, content: String) -> Result<(), String> {
    fs::write(&path, content)
        .map_err(|e| format!("Failed to write file: {}", e))
}

#[tauri::command]
pub async fn search_files(query: String, path: String) -> Result<Vec<FileItem>, String> {
    let base_path = PathBuf::from(&path);

    if !base_path.exists() {
        return Err(format!("Path does not exist: {}", base_path.display()));
    }

    let mut results = Vec::new();
    let query_lower = query.to_lowercase();

    for entry in WalkDir::new(&base_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let file_name = entry.file_name().to_string_lossy();
        if file_name.to_lowercase().contains(&query_lower) {
            let metadata = entry.metadata().ok();
            let modified = metadata.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0));

            results.push(FileItem {
                name: file_name.to_string(),
                path: entry.path().to_string_lossy().to_string(),
                is_dir: metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                size: metadata.and_then(|m| if m.is_file() { Some(m.len()) } else { None }),
                modified,
            });
        }
    }

    Ok(results)
}

#[tauri::command]
pub async fn pick_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    
    app.dialog()
        .file()
        .set_title("Select Repository Folder")
        .pick_folder(move |folder_path| {
            let result = folder_path.map(|p| p.to_string());
            let _ = tx.send(result);
        });
    
    match rx.await {
        Ok(Some(path)) => Ok(Some(path)),
        Ok(None) => Ok(None), // User cancelled
        Err(_) => Err("Dialog channel closed unexpectedly".to_string()),
    }
}

#[tauri::command]
pub async fn check_is_git_repo(path: String) -> Result<bool, String> {
    let git_path = PathBuf::from(&path).join(".git");
    Ok(git_path.exists())
}

#[tauri::command]
pub async fn get_file_info(path: String) -> Result<FileInfo, String> {
    let path_obj = Path::new(&path);

    let metadata = fs::metadata(path_obj)
        .map_err(|e| format!("Failed to get file metadata: {}", e))?;

    let created = metadata.created()
        .ok()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0))
        .unwrap_or_else(Utc::now);

    let modified = metadata.modified()
        .ok()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0))
        .unwrap_or_else(Utc::now);

    let readonly = metadata.permissions().readonly();
    let permissions = if readonly { "r--" } else { "rw-" };

    Ok(FileInfo {
        path: path.clone(),
        size: metadata.len(),
        created,
        modified,
        permissions: permissions.to_string(),
    })
}
