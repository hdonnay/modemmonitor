use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;

use clap::{App, Arg, ArgMatches};
use dirs;
use futures::{join, prelude::*, stream::FuturesUnordered, try_join};
use matrix_sdk::{
    self,
    events::room::message::{MessageEventContent, TextMessageEventContent},
    identifiers::RoomId,
    uuid::Uuid,
    JsonStore,
};
use regex::Regex;
use reqwest::{self, Url};
use smol::{blocking, reader, Timer};

static BIN_NAME: &str = env!("CARGO_PKG_NAME");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = &[
        Arg::with_name("modem")
            .short("m")
            .help("modem address")
            .default_value("http://192.168.100.1/"),
        Arg::with_name("reset")
            .short("r")
            .help("factory reset the modem if sending a reboot command"),
        Arg::with_name("uthreshold")
            .short("c")
            .long("count")
            .help("threshold count of uncorrectable errors")
            .default_value("1000"),
        Arg::with_name("cthreshold")
            .long("correct-count")
            .help("threshold count of correctable errors")
            .default_value("100000"),
        Arg::with_name("homeserver")
            .long("homeserver")
            .help("homeserver for matrix notifications")
            .default_value("https://synapse.hdonnay.net/"),
        Arg::with_name("dry-run").short("n").help("dry run"),
        Arg::with_name("dry-run-notify")
            .short("N")
            .help("dry run, but still notify"),
    ];
    let m = App::new(BIN_NAME)
        .author(env!("CARGO_PKG_AUTHORS"))
        .version(env!("CARGO_PKG_VERSION"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .args(args)
        .get_matches();
    let opts = Opts::try_from(&m)?;
    smol::run(app(opts))
}

async fn app(opts: Opts) -> Result<(), Box<dyn std::error::Error>> {
    let c = reqwest::Client::new();
    let sep = Regex::new("</?t[rd]></?t[rd]>").expect("programmer error");
    let mut cache = dirs::cache_dir().expect("no cache directory found");
    cache.push(BIN_NAME);
    let mut cfg = dirs::config_dir().expect("no config directory found");
    cfg.push(BIN_NAME);

    let (ct, mc) = try_join!(
        get_counts(&c, &opts.addr, &sep),
        matrix_setup(&opts.notification.homeserver, &cfg, &cache, opts.notify),
    )?;
    println!("found {} correctable errors", ct.correctable);
    println!("found {} uncorrectable errors", ct.uncorrectable);

    if ct.correctable < opts.correctable_threshold
        && ct.uncorrectable < opts.uncorrectable_threshold
    {
        return Ok(());
    }

    let body = opts.notification_message(&ct);
    let _ = join!(notifications(&mc, &body, opts.notify), async {
        println!("pausing for cancel....");
        Timer::after(Duration::from_secs(5)).await
    });
    println!("{}", opts.message());
    if opts.dry_run {
        return Ok(());
    }
    c.post(opts.addr.join("goform/RgConfiguration.pl")?)
        .form(&[("Rebooting", "1"), opts.reset_arg()])
        .send()
        .await?;
    Ok(())
}

struct Opts {
    addr: Url,
    dry_run: bool,
    reset: bool,
    notify: bool,
    correctable_threshold: u64,
    uncorrectable_threshold: u64,
    notification: NotificationOpts,
}

impl Opts {
    fn message(&self) -> String {
        if self.dry_run {
            String::from("would issue modem reboot")
        } else {
            format!(
                "issuing modem reboot{}",
                if self.reset { " and reset" } else { "" }
            )
        }
    }
    fn notification_message(&self, ct: &ErrorCount) -> String {
        format!(
            "Rebooting{} modem shortly: found {} correctable, {} uncorrectable errors{}.",
            if self.reset { " and resetting" } else { "" },
            ct.correctable,
            ct.uncorrectable,
            if self.dry_run {
                " (jk this is a dry run)"
            } else {
                ""
            }
        )
    }
    fn reset_arg(&self) -> (&str, &str) {
        ("RestoreFactoryDefault", if self.reset { "1" } else { "0" })
    }
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            addr: Url::parse("http://192.168.100.1/").unwrap(),
            dry_run: false,
            reset: false,
            notify: true,
            correctable_threshold: 100_000,
            uncorrectable_threshold: 1000,
            notification: Default::default(),
        }
    }
}

use std::convert::TryFrom;
impl TryFrom<&ArgMatches<'_>> for Opts {
    type Error = Box<dyn std::error::Error>;

    fn try_from(m: &ArgMatches) -> Result<Self, Self::Error> {
        let mut opts: Self = Default::default();
        if let Some(v) = m.value_of("modem") {
            opts.addr = v.parse()?;
        }
        if let Some(v) = m.value_of("uthreshold") {
            opts.uncorrectable_threshold = v.parse()?;
        }
        if let Some(v) = m.value_of("cthreshold") {
            opts.correctable_threshold = v.parse()?;
        }
        opts.dry_run = m.is_present("dry-run") || m.is_present("dry-run-notify");
        if m.is_present("dry-run") && !m.is_present("dry-run-notify") {
            opts.notify = false;
        }
        opts.reset = m.is_present("reset");
        if let Some(v) = m.value_of("homeserver") {
            opts.notification.homeserver = v.parse()?;
        }
        Ok(opts)
    }
}

struct NotificationOpts {
    homeserver: Url,
}

impl Default for NotificationOpts {
    fn default() -> Self {
        Self {
            homeserver: Url::parse("https://synapse.hdonnay.net/").unwrap(),
        }
    }
}

async fn get_counts(
    c: &reqwest::Client,
    addr: &Url,
    sep: &Regex,
) -> Result<ErrorCount, Box<dyn std::error::Error>> {
    let res = c.get(addr.join("")?).send().await?;
    let page = res.text().await?;
    let counts = page
        .split('\n')
        .filter_map(|l| {
            let fs: Vec<&str> = sep.split(l).collect();
            let l = fs.len();
            if l > 5 && fs[2] == "Locked" && fs[3] == "QAM256" {
                Some((
                    fs[l - 3].parse::<u64>().unwrap(),
                    fs[l - 2].parse::<u64>().unwrap(),
                ))
            } else {
                None
            }
        })
        .unzip();
    Ok(ErrorCount::from(counts))
}

struct ErrorCount {
    correctable: u64,
    uncorrectable: u64,
}

impl From<(Vec<u64>, Vec<u64>)> for ErrorCount {
    fn from(t: (Vec<u64>, Vec<u64>)) -> Self {
        let correctable = t.0.iter().sum();
        let uncorrectable = t.1.iter().sum();
        Self {
            correctable,
            uncorrectable,
        }
    }
}

async fn matrix_setup(
    homeserver: &Url,
    config: &Path,
    cache: &Path,
    notify: bool,
) -> Result<matrix_sdk::Client, Box<dyn std::error::Error>> {
    let mut cache = cache.to_path_buf();
    cache.push("store.json");
    let store = JsonStore::open(&cache)?;
    let matrix_cfg = matrix_sdk::ClientConfig::new().state_store(Box::new(store));
    let mc = matrix_sdk::Client::new_with_config(homeserver.clone(), matrix_cfg)?;
    if !notify {
        return Ok(mc);
    }

    let mut config = config.to_path_buf();
    config.push("session");
    let filename = config.clone();
    let _session_file = blocking!(OpenOptions::new()
        .write(true)
        .read(true)
        .create(true)
        .open(filename))?;
    /*
    let mut _session_r = reader(session_file);
    let mut _session_w = writer(session_file);
    */
    if mc.logged_in().await {
        return Ok(mc);
    }
    eprintln!("matrix client not logged in");
    config.set_file_name("config");
    let mut contents = String::new();
    let cfg_file = blocking!(OpenOptions::new()
        .write(true)
        .read(true)
        .create(true)
        .open(config))?;
    let mut cfg_file = reader(cfg_file);
    cfg_file.read_to_string(&mut contents).await?;
    let mut lines = contents.lines();
    let username = lines.next().unwrap_or("");
    let password = lines.next().unwrap_or("");
    let device_id = lines.next();
    let device_display_name = lines.next();

    mc.login(username, password, device_id, device_display_name)
        .await?;
    mc.sync(matrix_sdk::SyncSettings::default()).await?;
    for id in mc.invited_rooms().read().await.keys() {
        println!("joining room: {}", id);
        mc.join_room_by_id(id).await?;
    }
    // Currently no way to get a session out of a client.
    /*
    if let Some(s) = mc.session().read() {
        session_w.write_string("\n").await?;
    }
    */
    Ok(mc)
}

async fn notifications(
    c: &matrix_sdk::Client,
    body: &str,
    notify: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !notify {
        return Ok(());
    }
    let content = MessageEventContent::Text(TextMessageEventContent {
        body: body.to_owned(),
        format: None,
        formatted_body: None,
        relates_to: None,
    });
    let ids: Vec<RoomId> = c
        .joined_rooms()
        .read()
        .await
        .keys()
        .map(|id| id.clone())
        .collect();
    let mut msgs = ids
        .iter()
        .map(|id| {
            let txn_id = Uuid::new_v4();
            println!("queueing notification to room: {}", id);
            c.room_send(id, content.clone(), Some(txn_id))
        })
        .collect::<FuturesUnordered<_>>();
    while let Some(done) = msgs.next().await {
        done?;
    }
    Ok(())
}
