use std::{path::PathBuf, ffi::OsString};
use inotify::{Inotify, Event, WatchMask, EventMask};
use tokio_stream::StreamExt;
use serde::Deserialize;
use regex::Regex;
use chrono::prelude::*;
use std::fs;
use std::io;
use log::{info, warn, error, debug, as_debug};


type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize, Debug)]
struct Config {
    #[serde(rename="baseDir")]
    base_dir: PathBuf,
    #[serde(rename="watchDir")]
    watch_dir: String,
    rules: Vec<Rule>,
}

#[derive(Deserialize, Debug)]
struct Rule {
    #[serde(with = "serde_regex")]
    regex: Regex,
    #[serde(rename = "minSize")]
    min_size: Option<String>,
    #[serde(with = "serde_yaml::with::singleton_map_recursive")]
    actions: Vec<Action>,
}

#[derive(Deserialize, Debug)]
enum Action {
    #[serde(rename="move")]
    Move{dest: String, duplicate: DuplicateAction},
    #[serde(rename="unzip")]
    Unzip{dest: String},
    #[serde(rename="delete")]
    Delete,
}

#[derive(Deserialize, Debug)]
enum DuplicateAction {
    #[serde(rename="rename-date")]
    RenameDate,
    #[serde(rename="skip")]
    Skip,
    #[serde(rename="overwrite")]
    Overwrite,
}

struct SizeMatcher {
    matcher: Regex,
}

impl SizeMatcher {
    fn new() -> Result<Self> {
        Ok(SizeMatcher {
            matcher: Regex::new("^(?P<size>\\d+)(?P<units>\\w{0,2}$)")?,
        })
    }

    fn is_gteq(&self, file_size: u64, comparison: &str) -> Result<bool> {
        let (size, units) = if let Some(captures) = self.matcher.captures(comparison) {
            let raw_size = if let Some(raw_size) = captures.name("size") {
                raw_size
            } else {
                return Err("unable to find capture group [size]".into())
            };

            let units = if let Some(units) = captures.name("units") {
                units
            } else {
                return Err("unable to find capture group [units]".into())
            };

            (raw_size.as_str().parse::<u64>()?, units.as_str())
        } else {
            return Err(format!("size comparison string [{}] is not valid for regex [{}]", comparison, self.matcher.as_str()).into())
        };

        let size = match units {
            "" | "b" | "B" => size,
            "k" | "kb" | "Kb" | "KB" => size * 2u64.pow(10),
            "m" | "mb" | "Mb" | "MB" => size * 2u64.pow(20),
            "g" | "gb" | "Gb" | "GB" => size * 2u64.pow(20),
            "t" | "tb" | "Tb" | "TB" => size * 2u64.pow(20),
            v @ _ => return Err(format!("unknown unit specification {v}").into()),
        };

        Ok(file_size > size)
    }
}

struct Organiser {
    base_dir: PathBuf,
    watch_dir: PathBuf,
    rules: Vec<Rule>,
    size_matcher: SizeMatcher,
}

impl Organiser {
    async fn run(&self) -> Result<()> {
        let inotify = Inotify::init()?;
        inotify.watches().add(self.watch_dir.to_str().unwrap(), WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::ONLYDIR)?;
        let mut buffer = [0; 1024];
        let mut stream = inotify.into_event_stream(&mut buffer)?;

        info!(watch_dir=self.watch_dir.to_str(); "watching directory for file events");

        while let Some(event) = stream.next().await {
            match self.process_event(event).await {
                Ok(_) => { /* NO OP */ },
                Err(err) => {
                    error!(error=err; "encountered error processing event")
                },
            }
        }

        Ok(())
    }

    async fn process_event(&self, event: std::result::Result<Event<OsString>, std::io::Error>) -> Result<()> {
        let event = event?;

        debug!(event_type=as_debug!(event.mask), filename=as_debug!(event.name); "received filesystem event");

        if event.mask != EventMask::CLOSE_WRITE && event.mask != EventMask::MOVED_TO {
            return Ok(())
        }

        let rules = &self.rules;
        if let Some(raw_name) = event.name {
            let name = raw_name.to_str().unwrap().to_string();
            let source = self.watch_dir.join(&name);

            if !source.exists() {
                warn!(filename=name; "file does not exist - assuming processed by previous event, or checking if file is writable");
                return Ok(())
            }

            for rule in rules.iter() {
                if rule.regex.is_match(&name) {
                    debug!(regex=rule.regex.as_str(), filename=name; "rule matched regex for file");
                    if let Some(min_size) = &rule.min_size {
                        let file = std::fs::metadata(self.watch_dir.join(&name))?;
                        if !self.size_matcher.is_gteq(file.len(), &min_size)? {
                            info!(filename=name; "file is less than the minimum size for this rule - skipping rule");
                            continue;
                        }
                    }
                    for action in &rule.actions {
                        info!(action=as_debug!(action); "performing action");
                        match action {
                            Action::Move { dest, duplicate } => {
                                let dest = self.base_dir.join(dest).join(&name);
                                if dest.exists() {
                                    match duplicate {
                                        DuplicateAction::Skip => return Ok(()),
                                        DuplicateAction::Overwrite => std::fs::rename(&source, dest)?,
                                        DuplicateAction::RenameDate => {
                                            let date = Local::now().format("%Y-%m-%dT%H_%M_%S").to_string();
                                            let name = format!("{date}__{name}");
                                            std::fs::rename(&source, dest.parent().unwrap().join(name))?;
                                        },
                                    }
                                } else {
                                    std::fs::rename(&source, dest)?;
                                }
                            },
                            Action::Unzip { dest } => {
                                let dest = self.base_dir.join(&dest);
                                let fname = source.clone();
                                let file = fs::File::open(fname)?;

                                let mut archive = zip::ZipArchive::new(file)?;

                                
                                for i in 0..archive.len() {
                                    let mut file = archive.by_index(i)?;
                                    let outpath = match file.enclosed_name() {
                                        Some(path) => dest.join(path),
                                        None => continue,
                                    };

                                    {
                                        let comment = file.comment();
                                        if !comment.is_empty() {
                                            info!(file_index=i, comment=comment; "File comment");
                                        }
                                    }

                                    if (file.name()).ends_with('/') {
                                        info!(file_index=i, destination=outpath.to_str(); "File extracted");
                                        fs::create_dir_all(&outpath)?;
                                    } else {
                                        info!(
                                            file_index=i,
                                            destination=outpath.to_str(),
                                            file_size=file.size();
                                            "File extracted",
                                        );
                                        if let Some(p) = outpath.parent() {
                                            if !p.exists() {
                                                fs::create_dir_all(p)?;
                                            }
                                        }
                                        let mut outfile = fs::File::create(&outpath)?;
                                        io::copy(&mut file, &mut outfile)?;
                                    }

                                    // Get and Set permissions
                                    #[cfg(unix)]
                                    {
                                        use std::os::unix::fs::PermissionsExt;

                                        if let Some(mode) = file.unix_mode() {
                                            fs::set_permissions(&outpath, fs::Permissions::from_mode(mode))?;
                                        }
                                    }
                                }
                            },
                            Action::Delete => {
                                std::fs::remove_file(&source)?;
                            },
                        };
                        debug!(filename=name; "all actions for file processed successfully");
                        return Ok(())
                    }
                } else {
                    debug!(regex=rule.regex.as_str(), filename=name; "rule regex did not match file");
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    std_logger::Config::logfmt().init();
    let config_file = include_str!("rules.yml");
    let config: Config = serde_yaml::from_str(&config_file)?;
    
    let base_dir = PathBuf::from(&config.base_dir);
    let watch_dir = base_dir.join(&config.watch_dir);

    let organiser = Organiser {
        base_dir,
        watch_dir,
        rules: config.rules,
        size_matcher: SizeMatcher::new()?,
    };

    organiser.run().await?;

    Ok(())
}
