use std::path::{Path, PathBuf};
use chrono::{DateTime, TimeZone, Utc};
use git2::{Repository, Branch, DiffOptions, Oid};
use tracing::error;
use url::Url;
use crate::custom_error::MapErrToString;
use crate::files_correction::canonical_path;
use crate::git::{FileChange, FileChangeStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn status_options(include_unmodified: bool, show: git2::StatusShow) -> git2::StatusOptions {
    let mut options = git2::StatusOptions::new();
    options
        .disable_pathspec_match(true)
        .include_ignored(false)
        .include_unmodified(include_unmodified)
        .include_unreadable(false)
        .include_untracked(true)
        .recurse_ignored_dirs(false)
        .recurse_untracked_dirs(true)
        .rename_threshold(100)
        .update_index(true)
        .show(show);
    options
}

#[allow(dead_code)]
pub fn get_git_remotes(repository_path: &Path) -> Result<Vec<(String, String)>, String> {
    let repository = Repository::discover(repository_path)
        .map_err(|e| format!("Failed to open repository: {}", e))?;
    let remotes = repository.remotes()
        .map_err(|e| format!("Failed to get remotes: {}", e))?;
    let mut result = Vec::new();
    for name in remotes.iter().flatten() {
        if let Ok(remote) = repository.find_remote(name) {
            if let Some(url) = remote.url() {
                if let Ok(mut parsed_url) = Url::parse(url) {
                    parsed_url.set_username("").ok();
                    parsed_url.set_password(None).ok();
                    result.push((name.to_string(), parsed_url.to_string()));
                } else {
                    result.push((name.to_string(), url.to_string()));
                }                
            }
        }
    }
    Ok(result)
}

pub fn git_ls_files(repository_path: &PathBuf) -> Option<Vec<PathBuf>> {
    let repository = Repository::open(repository_path)
        .map_err(|e| error!("Failed to open repository: {}", e)).ok()?;

    let statuses = repository.statuses(Some(
        &mut status_options(true, git2::StatusShow::IndexAndWorkdir)))
        .map_err(|e| error!("Failed to get statuses: {}", e)).ok()?;

    let mut files = Vec::new();
    for entry in statuses.iter() {
        let path = String::from_utf8_lossy(entry.path_bytes()).to_string();
        files.push(repository_path.join(path));
    }
    if !files.is_empty() { Some(files) } else { None }
}

pub fn get_or_create_branch<'repo>(repository: &'repo Repository, branch_name: &str) -> Result<Branch<'repo>, String> {
    match repository.find_branch(branch_name, git2::BranchType::Local) {
        Ok(branch) => Ok(branch),
        Err(_) => {
            let head_commit = repository.head()
                .and_then(|h| h.peel_to_commit())
                .map_err_with_prefix("Failed to get HEAD commit:")?;
            repository.branch(branch_name, &head_commit, false)
                .map_err_with_prefix("Failed to create branch:")
        }
    }
}

fn is_changed_in_wt(status: git2::Status) -> bool {
    status.intersects(git2::Status::WT_NEW | 
        git2::Status::WT_MODIFIED | 
        git2::Status::WT_DELETED | 
        git2::Status::WT_RENAMED | 
        git2::Status::WT_TYPECHANGE)
}

fn is_changed_in_index(status: git2::Status) -> bool {
    status.intersects(git2::Status::INDEX_NEW | 
        git2::Status::INDEX_MODIFIED | 
        git2::Status::INDEX_DELETED | 
        git2::Status::INDEX_RENAMED | 
        git2::Status::INDEX_TYPECHANGE)
}

/// Returns (staged_changes, unstaged_changes), note that one of them may be always empty based on show_opt
/// 
/// If include_abs_path is true, they are included in the FileChanges result, use it if they need to be 
/// returned to the client or the absolute paths are needed
pub fn get_diff_statuses(show_opt: git2::StatusShow, repo: &Repository, include_abs_paths: bool) -> Result<(Vec<FileChange>, Vec<FileChange>), String> {
    let repo_workdir = repo.workdir()
        .ok_or("Failed to get workdir from repository".to_string())?;

    let mut staged_changes = Vec::new();
    let mut unstaged_changes = Vec::new();
    let statuses = repo.statuses(Some(&mut status_options(false, show_opt)))
        .map_err_with_prefix("Failed to get statuses:")?;
    
    for entry in statuses.iter() {
        let status = entry.status();
        let relative_path = PathBuf::from(String::from_utf8_lossy(entry.path_bytes()).to_string());
        
        if entry.path_bytes().last() == Some(&b'/') && repo_workdir.join(&relative_path).join(".git").exists() {
            continue;
        }

        let should_not_be_present = match show_opt {
            git2::StatusShow::Index => is_changed_in_wt(status) || status.is_index_renamed(),
            git2::StatusShow::Workdir => is_changed_in_index(status) || status.is_wt_renamed(),
            git2::StatusShow::IndexAndWorkdir => status.is_index_renamed() || status.is_wt_renamed(),
        };
        if should_not_be_present {
            tracing::error!("File status is {:?} for file {:?}, which should not be present due to status options.", status, relative_path);
            continue;
        }

        let absolute_path = if include_abs_paths && (is_changed_in_index(status) || is_changed_in_wt(status)) { 
            canonical_path(repo_workdir.join(&relative_path).to_string_lossy())
        } else {
            PathBuf::new()
        };

        if is_changed_in_index(status) {
            staged_changes.push(FileChange {
                status: match status {
                    s if s.is_index_new() => FileChangeStatus::ADDED,
                    s if s.is_index_deleted() => FileChangeStatus::DELETED,
                    _ => FileChangeStatus::MODIFIED,
                },
                absolute_path: absolute_path.clone(),
                relative_path: relative_path.clone(),
            });
        }

        if is_changed_in_wt(status) {
            unstaged_changes.push(FileChange {
                status: match status {
                    s if s.is_wt_new() => FileChangeStatus::ADDED,
                    s if s.is_wt_deleted() => FileChangeStatus::DELETED,
                    _ => FileChangeStatus::MODIFIED,
                },
                absolute_path,
                relative_path,
            });
        }
    }

    Ok((staged_changes, unstaged_changes))
}

pub fn get_diff_statuses_index_to_commit(repository: &Repository, commit_oid: &git2::Oid, include_abs_paths: bool) -> Result<Vec<FileChange>, String> {
    let head = repository.head().map_err_with_prefix("Failed to get HEAD:")?;
    let original_head_ref = head.is_branch().then(|| head.name().map(ToString::to_string)).flatten();
    let original_head_oid = head.target();

    repository.set_head_detached(commit_oid.clone()).map_err_with_prefix("Failed to set HEAD:")?;

    let result = get_diff_statuses(git2::StatusShow::Index, repository, include_abs_paths);

    let restore_result = match (&original_head_ref, original_head_oid) {
        (Some(head_ref), _) => repository.set_head(head_ref),
        (None, Some(oid)) => repository.set_head_detached(oid),
        (None, None) => Ok(()),
    };

    if let Err(restore_err) = restore_result {
        let prev_err = result.as_ref().err().cloned().unwrap_or_default();
        return Err(format!("{}\nFailed to restore head: {}", prev_err, restore_err));
    }

    result.map(|(staged_changes, _unstaged_changes)| staged_changes)
}

pub fn stage_changes(repository: &Repository, file_changes: &Vec<FileChange>, abort_flag: &Arc<AtomicBool>) -> Result<(), String> {
    let mut index = repository.index().map_err_with_prefix("Failed to get index:")?;

    for file_change in file_changes {
        // NOTE: this loop can take a lot of time (25s for linux) when we just init the repo
        if abort_flag.load(Ordering::SeqCst) {
            return Err("stage_changes aborted".to_string());
        }
        match file_change.status {
            FileChangeStatus::ADDED | FileChangeStatus::MODIFIED => {
                index.add_path(&file_change.relative_path)
                    .map_err_with_prefix("Failed to add file to index:")?;
            },
            FileChangeStatus::DELETED => {
                index.remove_path(&file_change.relative_path)
                    .map_err_with_prefix("Failed to remove file from index:")?;
            },
        }
    }

    index.write().map_err_with_prefix("Failed to write index:")?;
    Ok(())
}

pub fn get_configured_author_email_and_name(repository: &Repository) -> Result<(String, String), String> {
    let config = repository.config()
        .map_err_with_prefix("Failed to get repository config:")?;
    let author_email = config.get_string("user.email")
        .map_err_with_prefix("Failed to get author email:")?;
    let author_name = config.get_string("user.name")
        .map_err_with_prefix("Failed to get author name:")?;
    Ok((author_email, author_name))
}

pub fn commit(repository: &Repository, branch: &Branch, message: &str, author_name: &str, author_email: &str) -> Result<Oid, String> {
    let mut index = repository.index().map_err_with_prefix("Failed to get index:")?;
    let tree_id = index.write_tree().map_err_with_prefix("Failed to write tree:")?;
    let tree = repository.find_tree(tree_id).map_err_with_prefix("Failed to find tree:")?;

    let signature = git2::Signature::now(author_name, author_email)
        .map_err_with_prefix("Failed to create signature:")?;
    let branch_ref_name = branch.get().name().ok_or("Invalid branch name".to_string())?;

    let parent_commit = if let Some(target) = branch.get().target() {
        repository.find_commit(target)
            .map_err(|e| format!("Failed to find branch commit: {}", e))?
    } else {
        return Err("No parent commits found".to_string());
    };

    let commit = repository.commit(
        Some(branch_ref_name), &signature, &signature, message, &tree, &[&parent_commit]
    ).map_err(|e| format!("Failed to create commit: {}", e))?;

    repository.set_head(branch_ref_name).map_err_with_prefix("Failed to set branch as head:")?;

    Ok(commit)
}

pub fn open_or_init_repo(path: &Path) -> Result<Repository, String> {
    match Repository::open(path) {
        Ok(repo) => Ok(repo),
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            Repository::init(path).map_err_to_string()
        },
        Err(e) => Err(e.to_string()),
    }
}

pub fn get_commit_datetime(repository: &Repository, commit_oid: &Oid) -> Result<DateTime<Utc>, String> {
    let commit = repository.find_commit(commit_oid.clone()).map_err_to_string()?;

    Utc.timestamp_opt(commit.time().seconds(), 0).single()
        .ok_or_else(|| "Failed to get commit datetime".to_string())
}

pub fn git_diff_head_to_workdir<'repo>(repository: &'repo Repository) -> Result<git2::Diff<'repo>, String> {
    let mut diff_options = DiffOptions::new();
    diff_options.include_untracked(true);
    diff_options.recurse_untracked_dirs(true);

    let head = repository.head().and_then(|head_ref| head_ref.peel_to_tree())
        .map_err(|e| format!("Failed to get HEAD tree: {}", e))?;

    let diff = repository.diff_tree_to_workdir(Some(&head), Some(&mut diff_options))
        .map_err(|e| format!("Failed to generate diff: {}", e))?;
    
    Ok(diff)
}

pub fn git_diff_head_to_workdir_as_string(repository: &Repository, max_size: usize) -> Result<String, String> {
    let diff = git_diff_head_to_workdir(repository)?;

    let mut diff_str = String::new();
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        let line_content = std::str::from_utf8(line.content()).unwrap_or("");
        if diff_str.len() + line_content.len() < max_size {
            diff_str.push(line.origin());
            diff_str.push_str(line_content);
            if diff_str.len() > max_size {
                diff_str.truncate(max_size - 4);
                diff_str.push_str("...\n");
            }
        }
        true
    }).map_err(|e| format!("Failed to print diff: {}", e))?;

    Ok(diff_str)
}

pub fn checkout_head_and_branch_to_commit(repo: &Repository, branch_name: &str, commit_oid: &Oid) -> Result<(), String> {
    let commit = repo.find_commit(commit_oid.clone()).map_err_with_prefix("Failed to find commit:")?;

    let mut branch_ref = repo.find_branch(branch_name, git2::BranchType::Local)
        .map_err_with_prefix("Failed to get branch:")?.into_reference();
    branch_ref.set_target(commit.id(),"Restoring checkpoint")
        .map_err_with_prefix("Failed to update branch reference:")?;

    repo.set_head(&format!("refs/heads/{}", branch_name))
        .map_err_with_prefix("Failed to set HEAD:")?;

    let mut checkout_opts = git2::build::CheckoutBuilder::new();
    checkout_opts.force().update_index(true);
    repo.checkout_head(Some(&mut checkout_opts)).map_err_with_prefix("Failed to checkout HEAD:")?;

    Ok(())
}
