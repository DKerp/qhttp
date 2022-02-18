use crate::Handler;
use crate::{Body, HttpError};

use std::path::{Path, PathBuf};
use std::io::ErrorKind;

use tokio::fs::File;

use async_trait::async_trait;

use http::header::*;
use http::{StatusCode, HeaderValue};



/// Serves the files of a single `root` directory.
pub struct FileServer {
    root: PathBuf,
    index_files: Vec<PathBuf>,
    hide_hidden_files: bool,
}

impl FileServer {
    /// Create a server which serves the provided `root` directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::new_with_index_files(root, Vec::new())
    }

    /// Create a server which serves the provided `root` directory und uses
    /// the provided list of names of `index_files` to serve requests which
    /// point to a directory.
    pub fn new_with_index_files(
        root: impl Into<PathBuf>,
        index_files: impl Into<Vec<PathBuf>>,
    ) -> Self {
        let root = root.into();
        let index_files = index_files.into();

        Self {
            root,
            index_files,
            hide_hidden_files: true,
        }
    }

    /// Check if the server is configured to hide hidden files.
    pub fn hide_hidden_files(&self) -> bool {
        self.hide_hidden_files
    }

    /// Set if the server should hide hidden files.
    pub fn set_hide_hidden_files(&mut self, hide: bool) {
        self.hide_hidden_files = hide;
    }
}

#[async_trait]
impl Handler for FileServer {
    async fn handle(&self, req: http::Request<Body>) -> Result<http::Response<Body>, HttpError> {
        log::debug!("FileServer - request: {:?}", req);

        // Create the response object.
        let mut resp = http::Response::new(Body::default());

        // Retrieve the requested path.
        let path = req.uri().path();
        let path = path.trim_start_matches("/");

        // Security check - make sure the client does not try to read parent folders!
        if path.contains("../") {
            log::warn!("FileServer - Request contained dangerous path component (../), returning FORBIDDEN.");

            *resp.status_mut() = StatusCode::FORBIDDEN;
            resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

            return Ok(resp);
        }

        if self.hide_hidden_files {
            if path.starts_with(".") || path.contains("/.") {
                log::warn!("FileServer - A hidden file was requested, returning NOT FOUND.");

                *resp.status_mut() = StatusCode::NOT_FOUND;
                resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

                return Ok(resp);
            }
        }

        // Construct the full path.
        let fullpath = self.root.join(path);

        log::info!("FileServer - root: {:?}, path: {:?}, fullpath: {:?}", self.root, path, fullpath);

        // If this is a directory, try to find and send the index file.
        if fullpath.is_dir() {
            for index_file in self.index_files.iter() {
                let index_path = fullpath.join(index_file);

                if try_send_file(index_path, &mut resp).await {
                    return Ok(resp);
                }
            }

            log::debug!("FileServer - Could not find an index file. fullpath: {:?}", fullpath);

            *resp.status_mut() = StatusCode::NOT_FOUND;
            resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

            return Ok(resp);
        }

        // Otherwise try to send the file.
        if try_send_file(fullpath, &mut resp).await {
            return Ok(resp);
        }

        *resp.status_mut() = StatusCode::NOT_FOUND;
        resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

        Ok(resp)
    }
}

/// Tries to send the file given at the path. Returns false if could not be found and true
/// in any other case, with either the file added to the response or an appropriate error.
async fn try_send_file<P: AsRef<Path>>(path: P, resp: &mut http::Response<Body>) -> bool {
    // Open the file.
    let file = match File::open(&path).await {
        Ok(file) => file,
        Err(err) => {
            log::debug!("FileServer - Could not open file. err: {:?}", err);

            let code: StatusCode = match err.kind() {
                ErrorKind::NotFound => return false,
                ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };

            *resp.status_mut() = code;
            resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

            return true;
        }
    };

    // Try to guess the mime type.
    if let Some(mime_str) = mime_guess::from_path(&path).first_raw() {
        match HeaderValue::from_bytes(mime_str.as_bytes()) {
            Ok(content_type) => {
                resp.headers_mut().insert(http::header::CONTENT_TYPE, content_type);
            }
            Err(err) => log::error!("FileServer - Converting mime type to HeaderValue failed! err: {:?}", err),
        }
    }

    // Create the body out of the opened file.
    let body = match Body::from_file(file).await {
        Ok(body) => body,
        Err(err) => {
            log::error!("FileServer - Creating body out of file failed. err: {:?}", err);

            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            resp.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

            return true;
        }
    };

    // Add the body and return the response.
    *resp.body_mut() = body;

    true
}
