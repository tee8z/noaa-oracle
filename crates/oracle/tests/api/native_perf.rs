//! Manual fresh-process and publication-refresh benchmark. Run with the same
//! ORACLE_PERF_DATA corpus as perf.rs. The source corpus is never changed:
//! immutable data files are hard-linked into a temporary copy on its filesystem.

use daemon::publish::{Artifact, Publisher};
use nostr::{key::Keys, types::Url};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration as StdDuration, Instant},
};
use time::{Duration, OffsetDateTime, Time, format_description::well_known::Rfc3339};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn clone_data(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text.ends_with(".lock") || name_text == "prepare.lock" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            clone_data(&path, &target.join(name));
        } else if path
            .extension()
            .is_some_and(|e| e == "parquet" || e == "duckdb")
        {
            fs::hard_link(&path, target.join(name)).unwrap();
        } else {
            fs::copy(&path, target.join(name)).unwrap();
        }
    }
}

fn get(address: SocketAddr, path: &str) -> std::io::Result<(f64, Vec<u8>)> {
    let began = Instant::now();
    let mut stream = TcpStream::connect_timeout(&address, StdDuration::from_secs(2))?;
    stream.set_read_timeout(Some(StdDuration::from_secs(30)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&bytes[..split]).unwrap();
    assert!(headers.starts_with("HTTP/1.1 200"), "{path}: {headers}");
    assert!(!headers.to_ascii_lowercase().contains("transfer-encoding"));
    Ok((
        began.elapsed().as_secs_f64() * 1000.0,
        bytes[split + 4..].to_vec(),
    ))
}

fn sample_size(path: &Path) -> Option<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Some(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("sample {}: {error}", path.display()),
    }
}

fn rss(pid: u32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ORACLE_PERF_DATA, a built oracle binary, and an idle builder"]
async fn native_fresh_http_and_new_publication() {
    let source = std::env::var("ORACLE_PERF_DATA").expect("ORACLE_PERF_DATA");
    let source = Path::new(&source);
    let directory = tempfile::Builder::new()
        .prefix("oracle-native-http-")
        .tempdir_in(source.parent().unwrap())
        .unwrap();
    let weather = directory.path().join("weather");
    clone_data(source, &weather);
    let config = directory.path().join("oracle.toml");
    fs::write(&config, "").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let origin = format!("http://{address}");
    let keys = Keys::generate();
    let log = fs::File::create(directory.path().join("server.log")).unwrap();
    let log_level = std::env::var("ORACLE_PERF_LOG_LEVEL").unwrap_or_else(|_| "info".to_owned());
    let began = Instant::now();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_oracle"))
            .args([
                "--config",
                config.to_str().unwrap(),
                "--host",
                "127.0.0.1",
                "--port",
                &address.port().to_string(),
                "--remote-url",
                &origin,
                "--weather-dir",
                weather.to_str().unwrap(),
                "--event-db",
                directory.path().join("events").to_str().unwrap(),
                "--oracle-private-key",
                directory.path().join("key.pem").to_str().unwrap(),
                "--uploader-pubkeys",
                &keys.public_key().to_hex(),
                "--level",
                &log_level,
            ])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    while get(address, "/ready").is_err() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited: {}",
            fs::read_to_string(directory.path().join("server.log")).unwrap()
        );
        assert!(
            began.elapsed() < StdDuration::from_secs(550),
            "startup timed out"
        );
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    println!(
        "fresh readiness: {:.1}ms",
        began.elapsed().as_secs_f64() * 1000.0
    );
    let today = OffsetDateTime::now_utc().replace_time(Time::MIDNIGHT);
    let at = |v: OffsetDateTime| v.format(&Rfc3339).unwrap();
    let broad = format!(
        "/stations/forecasts?station_ids={}&start={}&end={}",
        super::perf::AIRPORTS,
        at(today),
        at(today + Duration::days(1))
    );
    let (first, before) = get(address, &broad).unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|row| row["station_id"] == "KBTV"),
        "benchmark corpus must contain current KBTV forecasts"
    );
    println!(
        "fresh broad90 HTTP: {first:.1}ms, {} bytes, RSS high-water {}KiB",
        before.len(),
        rss(server.0.id())
    );
    assert!(
        first < 400.0,
        "fresh prepared broad90 exceeded target: {first:.1}ms"
    );
    for path in [
        "/",
        "/fragments/weather?view=list",
        "/fragments/forecast/KSAW",
    ] {
        let elapsed = get(address, path).unwrap().0;
        println!("fresh HTTP {path}: {elapsed:.1}ms");
        assert!(elapsed < 400.0, "{path} exceeded target: {elapsed:.1}ms");
    }
    let start = today - Duration::hours(21);
    let end = start + Duration::days(1);
    let forecasts = format!(
        "/stations/forecasts?station_ids=KPWM,KBTV,KBED&start={}&end={}&generated_start={}&generated_end={}&temperature_unit=fahrenheit",
        at(start),
        at(end),
        at(start - Duration::days(7)),
        at(start - Duration::nanoseconds(1))
    );
    let observations = format!(
        "/stations/observations?station_ids=KPWM,KBTV,KBED&start={}&end={}&temperature_unit=fahrenheit",
        at(start),
        at(end - Duration::nanoseconds(1))
    );
    let sequential = Instant::now();
    let obs = get(address, &observations).unwrap().0;
    let (forecast, as_of_before) = get(address, &forecasts).unwrap();
    let as_of_rows: serde_json::Value = serde_json::from_slice(&as_of_before).unwrap();
    assert!(
        !as_of_rows.as_array().unwrap().is_empty(),
        "benchmark needs nonempty as-of forecasts"
    );
    println!(
        "coordinator exact as-of HTTP: observations {obs:.1}ms, forecasts {forecast:.1}ms, sequential {:.1}ms",
        sequential.elapsed().as_secs_f64() * 1000.0
    );

    // A real signed daemon upload carries a newly issued forecast correction.
    // Read it immediately, throughout the ordinary background rebuild, and after
    // the new immutable manifest is published. No response may lose the update.
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        now.date(),
        today.date(),
        "rerun benchmark after UTC midnight rollover"
    );
    let stage = directory.path().join("upload").join(now.date().to_string());
    fs::create_dir_all(&stage).unwrap();
    let new_name = format!("forecasts_{}.parquet", at(now));
    let new_path = stage.join(&new_name);
    assert!(
        !weather
            .join(now.date().to_string())
            .join(&new_name)
            .exists(),
        "upload must use a new immutable source path"
    );
    let connection = duckdb::Connection::open_in_memory().unwrap();
    connection.execute_batch(&format!(
        "COPY (SELECT 'KBTV' AS station_id, '{}' AS begin_time, '{}' AS end_time, '{}' AS generated_at,
          30::BIGINT AS min_temp, 123::BIGINT AS max_temp, 'fahrenheit' AS temperature_unit_code,
          5::BIGINT AS wind_speed) TO '{}' (FORMAT PARQUET)",
        at(today), at(today + Duration::hours(6)), at(now - Duration::seconds(1)), new_path.to_string_lossy().replace('\'', "''"))).unwrap();
    drop(connection);
    let prepared = weather.join("derived/prepared-current-v1");
    let manifest = prepared.join(format!("{}.json", today.date()));
    let expected_source = format!("{}/{}", now.date(), new_name);
    let previous: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert!(
        !previous["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source == &expected_source)
    );
    let publisher = Publisher::new(
        Url::parse(&origin).unwrap(),
        keys,
        None,
        slog::Logger::root(slog::Discard, slog::o!()),
    )
    .unwrap();
    let refreshing = Instant::now();
    publisher
        .publish(&Artifact::from_path(&new_path).unwrap())
        .await
        .unwrap();
    println!(
        "signed upload: {:.1}ms",
        refreshing.elapsed().as_secs_f64() * 1000.0
    );
    let mut timings = Vec::new();
    let mut max_spill = 0;
    let mut max_build = 0;
    loop {
        let (elapsed, body) = get(address, &broad).unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            rows.as_array()
                .unwrap()
                .iter()
                .any(|row| row["station_id"] == "KBTV" && row["temp_high"] == 123),
            "new publication disappeared: {rows}"
        );
        timings.push(elapsed);
        if elapsed >= 350.0 {
            let building = fs::read_dir(&prepared).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("building-")
            });
            println!(
                "slow foreground: {elapsed:.1}ms at {:.3}s, native building={building}",
                refreshing.elapsed().as_secs_f64()
            );
        }
        for entry in fs::read_dir(&prepared).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with("spill-") {
                let children = match fs::read_dir(entry.path()) {
                    Ok(children) => children,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => panic!("sample spill directory: {error}"),
                };
                let used: u64 = children
                    .map(|entry| entry.unwrap().path())
                    .filter_map(|path| sample_size(&path))
                    .sum();
                max_spill = max_spill.max(used);
            } else if entry.file_name().to_string_lossy().starts_with("building-") {
                max_build = max_build.max(sample_size(&entry.path()).unwrap_or(0));
            }
        }
        let current: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        if current["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source == &expected_source)
        {
            break;
        }
        assert!(
            refreshing.elapsed() < StdDuration::from_secs(300),
            "refresh timed out; logs at {}",
            directory.path().display()
        );
        tokio::time::sleep(StdDuration::from_secs(2)).await;
    }
    let first_update = timings[0];
    let max = timings.iter().copied().fold(0.0, f64::max);
    println!(
        "refresh: {:.3}s, {} foreground calls, first update {first_update:.1}ms, max {max:.1}ms, server RSS high-water {}KiB, sampled spill {max_spill} bytes, building {max_build} bytes",
        refreshing.elapsed().as_secs_f64(),
        timings.len(),
        rss(server.0.id())
    );
    let (final_ms, final_body) = get(address, &broad).unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&final_body).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|row| row["station_id"] == "KBTV" && row["temp_high"] == 123),
        "published native generation must retain correction"
    );
    println!("refreshed broad90 HTTP: {final_ms:.1}ms");
    assert!(
        final_ms < 400.0,
        "refreshed native query exceeded target: {final_ms:.1}ms"
    );
    assert_eq!(
        as_of_before,
        get(address, &forecasts).unwrap().1,
        "new issue must not change an earlier as-of response"
    );
    let contents = fs::read_to_string(directory.path().join("server.log")).unwrap();
    for line in contents.lines().filter(|l| {
        l.contains("native forecasts")
            || l.contains("weather preparation")
            || l.contains("slow weather query")
            || l.contains("slow forecast request")
            || l.contains("slow HTTP phases")
    }) {
        println!("{line}");
    }
    assert!(
        max < 400.0,
        "foreground query during preparation exceeded target: {max:.1}ms"
    );
}
