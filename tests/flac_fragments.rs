//! The decoder must give back the exact integer samples the reference FLAC
//! decoder gives, from the first frame and from the middle of a stream.

use sha2::{Digest, Sha256};
use std::path::Path;

#[test]
fn every_fixture_decodes_to_the_reference_samples() {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("flac.json")).unwrap()).unwrap();
    let fixtures = manifest.as_array().unwrap();
    assert!(fixtures.len() >= 4, "the generator writes two resolutions, whole and from the middle");
    for fixture in fixtures {
        let name = fixture["file"].as_str().unwrap();
        let data = std::fs::read(folder.join(name)).unwrap();
        let (bits, rate, frames) = (fixture["bits"].as_u64().unwrap() as u32, fixture["rate"].as_u64().unwrap() as u32, fixture["frames"].as_u64().unwrap());
        let (pcm, channels) = tidal_plugin::flac::decode(data.clone(), bits, rate, frames).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(channels, 2, "{name}");
        let digest: String = Sha256::digest(&pcm).iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(digest, fixture["pcm_sha256"].as_str().unwrap(), "{name}: samples differ from the reference decoder");
        assert_eq!(tidal_plugin::flac::stream_info(&data).map(|info| (info.1, info.2)), Some((bits, rate)), "{name}");
        assert!(tidal_plugin::flac::decode(data, bits, rate, frames + 1).is_err(), "{name}: a short fragment must be refused");
    }
}

/// Decoding cost per second of audio, for the streams in `CHILPA_FLAC_BENCH`
/// (a folder of `seg-<rate>-<bits>.flac` files). Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn decoding_cost_per_second_of_audio() {
    let Some(folder) = std::env::var_os("CHILPA_FLAC_BENCH") else { return };
    let mut paths: Vec<_> = std::fs::read_dir(folder).unwrap().map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "flac")).collect();
    paths.sort();
    for path in paths {
        let data = std::fs::read(&path).unwrap();
        let (_, bits, rate) = tidal_plugin::flac::stream_info(&data).unwrap();
        let info = &data[8..42];
        let total = ((u64::from(info[13]) & 0x0f) << 32) | u64::from(u32::from_be_bytes([info[14], info[15], info[16], info[17]]));
        let mut best = f64::MAX;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            tidal_plugin::flac::decode(data.clone(), bits, rate, total).unwrap();
            best = best.min(started.elapsed().as_secs_f64());
        }
        let seconds = total as f64 / f64::from(rate);
        println!("{}: {:.1} ms for {:.1} s of audio, {:.2} ms per second", path.file_name().unwrap().to_string_lossy(), best * 1000.0, seconds, best * 1000.0 / seconds);
    }
}
