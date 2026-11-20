//! The remote that names a repository, whichever directory holds a clone of it.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::workstation::Checkout;

/// A remote's address as host, owner, and name, without its scheme, its user,
/// and the ending `.git`. Each form of one repository's address gives one text.
pub fn normalize(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = match url.split_once("://") {
        // A URL: the host follows the scheme and an optional user.
        Some((_, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            (host, path)
        }
        // The short form of SSH: a user, a host, a colon, and the path.
        None => match url.split_once(':') {
            Some((authority, path)) if !authority.contains('/') => {
                let host = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                (host, path)
            }
            _ => return url.to_owned(),
        },
    };
    format!(
        "{}/{}",
        host.to_ascii_lowercase(),
        path.trim_start_matches('/')
    )
}

/// The address of the remote `origin` in the text of a repository's configuration.
pub fn parse(config: &str) -> Option<String> {
    let mut inside = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line.replace(' ', "") == "[remote\"origin\"]";
        } else if inside {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "url" {
                return Some(normalize(value));
            }
        }
    }
    None
}

/// Gives each checkout the remote `origin` of its repository. Git keeps the
/// remote in the main checkout's configuration, which the function reads once
/// for each repository. A repository without that remote keeps none.
pub fn fill(checkouts: &mut [Checkout]) {
    let mut found: HashMap<String, Option<String>> = HashMap::new();
    for checkout in checkouts {
        let repository = checkout.repository.as_ref().unwrap_or(&checkout.path);
        let origin = found.entry(repository.clone()).or_insert_with(|| {
            let config = Path::new(repository).join(".git").join("config");
            parse(&fs::read_to_string(config).ok()?)
        });
        checkout.origin.clone_from(origin);
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize, parse};

    #[test]
    fn each_form_of_an_address_names_one_repository() {
        let name = "github.com/pithecene/suds";
        assert_eq!(normalize("git@github.com:pithecene/suds.git"), name);
        assert_eq!(normalize("https://github.com/pithecene/suds"), name);
        assert_eq!(normalize("ssh://git@GitHub.com/pithecene/suds.git/"), name);
        // A path on this machine stays as its owner wrote it.
        assert_eq!(normalize("/srv/git/suds.git"), "/srv/git/suds");
    }

    #[test]
    fn the_configuration_names_the_origin_among_its_remotes() {
        let config = "[core]\n\tbare = false\n[remote \"fork\"]\n\turl = git@github.com:a/b.git\n[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\turl = https://github.com/pithecene/suds.git\n";
        assert_eq!(parse(config).as_deref(), Some("github.com/pithecene/suds"));
        assert_eq!(parse("[remote \"fork\"]\n\turl = x\n"), None);
    }
}
