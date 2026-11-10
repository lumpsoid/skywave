//! Workdir completion for the new-session prompt: which server directory
//! to list for what the user typed, and which paths complete it.
//!
//! The input splits at its last `/` into a directory and a name prefix. An
//! absolute input lists that directory; a relative one lists the roots and
//! matches them, and recent workdirs, by any path component. Only the last
//! listing is kept, for one directory: further edits within it filter the
//! same names, and the server is asked again only when the directory
//! changes, the same input is completed twice, or the listing failed. Until
//! a listing arrives, and whenever there is none, recent workdirs from the
//! replica stand in. Names starting with `.` show only when the prefix
//! does. Everything here is in memory.

use skywave_proto::DirsResult;

use crate::seam::Request;
use crate::transport::TransportError;
use crate::view::{Candidate, CandidateSource, Completion, CompletionStatus};

/// The prompt's completion while the session list is on screen; the
/// list's place holds it, so leaving the list drops it. Request ids come
/// from the model, which keeps them unused across visits.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Completer {
    /// The input being completed; `None` while no prompt completes.
    input: Option<String>,
    listing: Option<Listing>,
}

/// The last listing asked for.
#[derive(Debug, Clone, PartialEq)]
struct Listing {
    /// As asked for: the input up to its last `/`, or empty for the roots.
    dir: String,
    req: u64,
    /// Asked for and not answered yet.
    pending: bool,
    /// The latest answer for `dir`; `None` for a failed or missing one.
    result: Option<DirsResult>,
}

impl Completer {
    /// Completes `input`; returns the listing to ask the server for, if
    /// any, its id taken from `last_req` (the latest id handed out, never
    /// reused). Nothing is asked while the link is down (`online` false).
    pub(crate) fn complete(
        &mut self,
        input: String,
        online: bool,
        last_req: &mut u64,
    ) -> Option<Request> {
        let dir = dir_of(&input).to_owned();
        let repeated = self.input.as_deref() == Some(input.as_str());
        self.input = Some(input);
        let same_dir = self.listing.as_ref().filter(|l| l.dir == dir);
        if !online {
            if same_dir.is_none() {
                self.listing = None;
            }
            return None;
        }
        let ask = match same_dir {
            None => true,
            Some(listing) => repeated || (!listing.pending && listing.result.is_none()),
        };
        if !ask {
            return None;
        }
        *last_req += 1;
        let req = *last_req;
        let result = same_dir.and_then(|l| l.result.clone());
        self.listing = Some(Listing {
            dir: dir.clone(),
            req,
            pending: true,
            result,
        });
        Some(Request::ListDirs { req, dir })
    }

    /// The prompt closed: forget the input and the listing.
    pub(crate) fn end(&mut self) {
        self.input = None;
        self.listing = None;
    }

    /// The server's answer to request `req`; a stale one is ignored.
    pub(crate) fn listed(&mut self, req: u64, result: Result<DirsResult, TransportError>) {
        let Some(listing) = self.listing.as_mut().filter(|l| l.req == req) else {
            return;
        };
        listing.pending = false;
        listing.result = match result {
            Ok(result) => Some(result),
            Err(error) => {
                tracing::debug!(%error, dir = listing.dir, "listing directories failed");
                None
            }
        };
    }

    /// The candidates for the current input; `recents` are the replica's
    /// workdirs, most recently active first.
    pub(crate) fn view<'a>(&self, recents: impl Iterator<Item = &'a str>) -> Option<Completion> {
        let input = self.input.as_deref()?;
        let mut recent: Vec<&str> = Vec::new();
        for path in recents.filter(|path| matches(input, path)) {
            if !recent.contains(&path) {
                recent.push(path);
            }
        }
        let candidates = |paths: Vec<String>, source| {
            paths
                .into_iter()
                .map(|path| Candidate { path, source })
                .collect::<Vec<_>>()
        };
        let recent_only = || {
            candidates(
                recent.iter().map(|&p| p.to_owned()).collect(),
                CandidateSource::Recent,
            )
        };
        let (candidates, status) = match &self.listing {
            None => (recent_only(), CompletionStatus::Unavailable),
            Some(listing) => {
                let status = |truncated| match (listing.pending, &listing.result) {
                    (true, _) => CompletionStatus::Loading,
                    (false, None) => CompletionStatus::Unavailable,
                    (false, Some(_)) => CompletionStatus::Ready { truncated },
                };
                match &listing.result {
                    None => (recent_only(), status(false)),
                    Some(DirsResult::Roots { roots } | DirsResult::NotAllowed { roots }) => {
                        let roots: Vec<String> = roots
                            .iter()
                            .filter(|root| matches(input, root))
                            .cloned()
                            .collect();
                        let mut list = candidates(roots, CandidateSource::Server);
                        // A relative input has no directory to list: the
                        // roots and the recent workdirs are all it matches.
                        if !input.starts_with('/') {
                            for path in &recent {
                                if !list.iter().any(|c| c.path == *path) {
                                    list.push(Candidate {
                                        path: (*path).to_owned(),
                                        source: CandidateSource::Recent,
                                    });
                                }
                            }
                        }
                        (list, status(false))
                    }
                    Some(DirsResult::Listed {
                        names, truncated, ..
                    }) => {
                        let (dir, prefix) = input.split_at(dir_of(input).len());
                        let shown = |name: &&String| {
                            name.starts_with(prefix)
                                && (!name.starts_with('.') || prefix.starts_with('.'))
                        };
                        let paths = names
                            .iter()
                            .filter(shown)
                            .map(|name| format!("{dir}{name}"))
                            .collect();
                        (
                            candidates(paths, CandidateSource::Server),
                            status(*truncated),
                        )
                    }
                    Some(DirsResult::NotFound) => (Vec::new(), status(false)),
                }
            }
        };
        Some(Completion {
            input: input.to_owned(),
            candidates,
            status,
        })
    }
}

/// The directory part of `input`: up to and including its last `/` if it
/// is absolute, else empty (the roots).
fn dir_of(input: &str) -> &str {
    if !input.starts_with('/') {
        return "";
    }
    let end = input.rfind('/').map_or(0, |i| i + 1);
    &input[..end]
}

/// `path` completes `input`: extends it if absolute, else contains it from
/// the start of a path component on. An empty input matches everything.
fn matches(input: &str, path: &str) -> bool {
    if input.is_empty() {
        return true;
    }
    if input.starts_with('/') {
        return path.starts_with(input);
    }
    path.match_indices(input)
        .any(|(i, _)| i > 0 && path.as_bytes()[i - 1] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(completion: &Completion) -> Vec<(&str, CandidateSource)> {
        completion
            .candidates
            .iter()
            .map(|c| (c.path.as_str(), c.source))
            .collect()
    }

    fn listed(dir: &str, names: &[&str]) -> DirsResult {
        DirsResult::Listed {
            dir: dir.into(),
            names: names.iter().map(|&n| n.to_owned()).collect(),
            truncated: false,
        }
    }

    #[test]
    fn splits_input_into_dir_and_prefix() {
        assert_eq!(dir_of("/srv/co"), "/srv/");
        assert_eq!(dir_of("/srv/"), "/srv/");
        assert_eq!(dir_of("/"), "/");
        assert_eq!(dir_of("code/x"), "");
        assert_eq!(dir_of(""), "");
        assert!(matches("code/pro", "/srv/code/projects"));
        assert!(!matches("ode", "/srv/code"));
        assert!(matches("", "/srv/code"));
    }

    #[test]
    fn listing_is_filtered_by_prefix_and_hides_dotfiles() {
        let mut c = Completer::default();
        let mut last_req = 0;
        let Some(Request::ListDirs { req, dir }) = c.complete("/srv/".into(), true, &mut last_req)
        else {
            panic!("asks the server");
        };
        assert_eq!(dir, "/srv/");
        c.listed(req, Ok(listed("/srv", &[".git", "api", "app", "web"])));
        let view = c.view(std::iter::empty()).unwrap();
        assert_eq!(view.status, CompletionStatus::Ready { truncated: false });
        assert_eq!(
            paths(&view),
            [
                ("/srv/api", CandidateSource::Server),
                ("/srv/app", CandidateSource::Server),
                ("/srv/web", CandidateSource::Server),
            ]
        );
        // Same dir: the listing is reused, nothing is asked.
        assert_eq!(c.complete("/srv/a".into(), true, &mut last_req), None);
        let view = c.view(std::iter::empty()).unwrap();
        assert_eq!(view.candidates.len(), 2);
        assert_eq!(view.fill().as_deref(), Some("/srv/ap"));
        assert_eq!(c.complete("/srv/.".into(), true, &mut last_req), None);
        assert_eq!(
            paths(&c.view(std::iter::empty()).unwrap()),
            [("/srv/.git", CandidateSource::Server)]
        );
    }

    #[test]
    fn fill_takes_the_only_candidate_or_the_common_prefix() {
        let completion = |input: &str, paths: &[&str]| Completion {
            input: input.into(),
            candidates: paths
                .iter()
                .map(|&p| Candidate {
                    path: p.into(),
                    source: CandidateSource::Server,
                })
                .collect(),
            status: CompletionStatus::Ready { truncated: false },
        };
        assert_eq!(
            completion("/srv/a", &["/srv/api"]).fill().as_deref(),
            Some("/srv/api/")
        );
        assert_eq!(
            completion("/srv/p", &["/srv/project-a", "/srv/project-b"])
                .fill()
                .as_deref(),
            Some("/srv/project-")
        );
        assert_eq!(completion("/srv/", &["/srv/a", "/srv/b"]).fill(), None);
        assert_eq!(completion("/srv/", &[]).fill(), None);
        assert_eq!(
            completion("/é", &["/éa", "/éb"]).fill(),
            None,
            "multi-byte characters split cleanly"
        );
    }
}
