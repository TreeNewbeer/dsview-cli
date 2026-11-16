use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use dsview_sys::{
    decode_runtime_library_name, runtime_library_name, session_send_logic_chunk,
    source_decode_runtime_library_path, source_runtime_library_path, upstream_header_path,
    DecodeDecoder, DecodeExecutionLogicFormat, DecodeExecutionSession, DecodeRuntimeBridge,
    DecodeOptionValueKind, DecodeRuntimeError, DecodeRuntimeErrorCode, DecodeSessionChannelBinding,
    DecodeSessionInstance, DecodeSessionOption, DecodeSessionOptionValue, DeviceHandle,
    DslChannelSpec, DslExportRequest, ExportErrorCode, RuntimeBridge, RuntimeError,
    VcdExportRequest,
};

fn runtime_test_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

fn decode_diag_enabled() -> bool {
    std::env::var_os("DSVIEW_DECODE_DEBUG").is_some()
}

fn decode_diag(message: impl AsRef<str>) {
    if decode_diag_enabled() {
        eprintln!("[decode-diag] {}", message.as_ref());
    }
}

fn load_runtime() -> Option<RuntimeBridge> {
    let path = source_runtime_library_path()?;
    RuntimeBridge::load(path).ok()
}

fn skip_windows_vcd_goldens() -> bool {
    cfg!(target_os = "windows")
}

fn load_decode_runtime() -> Option<DecodeRuntimeBridge> {
    let path = source_decode_runtime_library_path()?;
    DecodeRuntimeBridge::load(path).ok()
}

fn i2c_root_instance() -> DecodeSessionInstance {
    DecodeSessionInstance {
        decoder_id: "1:i2c".to_string(),
        channel_bindings: vec![
            DecodeSessionChannelBinding {
                channel_id: "scl".to_string(),
                channel_index: 0,
            },
            DecodeSessionChannelBinding {
                channel_id: "sda".to_string(),
                channel_index: 1,
            },
        ],
        options: vec![],
    }
}

fn eeprom_stack_instance() -> DecodeSessionInstance {
    DecodeSessionInstance {
        decoder_id: "eeprom24xx".to_string(),
        channel_bindings: vec![],
        options: vec![DecodeSessionOption {
            option_id: "addr_counter".to_string(),
            value: DecodeSessionOptionValue::Integer(0),
        }],
    }
}

fn i2c_logic_sample(scl: bool, sda: bool) -> u8 {
    (u8::from(scl)) | (u8::from(sda) << 1)
}

fn push_i2c_state(samples: &mut Vec<u8>, scl: bool, sda: bool) {
    samples.push(i2c_logic_sample(scl, sda));
}

fn append_i2c_start(samples: &mut Vec<u8>) {
    push_i2c_state(samples, true, true);
    push_i2c_state(samples, true, false);
    push_i2c_state(samples, false, false);
}

fn append_i2c_bit(samples: &mut Vec<u8>, bit: bool) {
    push_i2c_state(samples, false, bit);
    push_i2c_state(samples, true, bit);
    push_i2c_state(samples, true, bit);
    push_i2c_state(samples, false, bit);
}

fn append_i2c_ack(samples: &mut Vec<u8>, ack: bool) {
    append_i2c_bit(samples, !ack);
}

fn append_i2c_byte(samples: &mut Vec<u8>, byte: u8) {
    for bit_index in (0..8).rev() {
        append_i2c_bit(samples, ((byte >> bit_index) & 1) != 0);
    }
}

fn append_i2c_stop(samples: &mut Vec<u8>) {
    push_i2c_state(samples, false, false);
    push_i2c_state(samples, true, false);
    push_i2c_state(samples, true, true);
    push_i2c_state(samples, true, true);
}

fn eeprom_byte_write_trace() -> Vec<u8> {
    let mut samples = Vec::new();
    append_i2c_start(&mut samples);
    append_i2c_byte(&mut samples, 0xA0);
    append_i2c_ack(&mut samples, true);
    append_i2c_byte(&mut samples, 0x00);
    append_i2c_ack(&mut samples, true);
    append_i2c_byte(&mut samples, 0xAB);
    append_i2c_ack(&mut samples, true);
    append_i2c_stop(&mut samples);
    samples
}

fn source_decoder_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root should exist")
        .join("DSView")
        .join("libsigrokdecode4DSL")
        .join("decoders")
}

fn vcd_string(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("synthetic VCD should be utf-8")
}

fn normalize_vcd(vcd: &str) -> String {
    let mut normalized = Vec::new();
    let mut lines = vcd.lines();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("$date") {
            normalized.push("$date <normalized> $end".to_string());
            continue;
        }
        if trimmed.starts_with("$version") {
            normalized.push("$version <normalized> $end".to_string());
            continue;
        }
        if trimmed == "$comment" {
            normalized.push("$comment".to_string());
            normalized.push("<normalized>".to_string());
            for comment_line in lines.by_ref() {
                if comment_line.trim() == "$end" {
                    normalized.push("$end".to_string());
                    break;
                }
            }
            continue;
        }
        normalized.push(trimmed.to_string());
    }

    normalized.join("\n") + "\n"
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("goldens")
        .join(name)
}

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("dsview-sys-{name}-{nonce}.vcd"))
}

fn parse_transitions(vcd: &str) -> Vec<(u64, String)> {
    let mut now = 0_u64;
    let mut transitions = Vec::new();

    for line in vcd.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(rest) = line.strip_prefix('#') {
            let mut parts = rest.split_whitespace();
            let timestamp = parts.next().expect("timestamp should exist");
            now = timestamp.parse().expect("timestamp should parse");
            for value in parts {
                transitions.push((now, value.to_string()));
            }
            continue;
        }
        if matches!(line, "$dumpvars" | "$end") || line.starts_with('$') {
            continue;
        }
        if matches!(line.as_bytes().first(), Some(b'0' | b'1' | b'x' | b'z')) {
            transitions.push((now, line.to_string()));
        }
    }

    transitions
}

fn final_timestamp(vcd: &str) -> Option<u64> {
    vcd.lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix('#'))
        .filter_map(|raw| raw.split_whitespace().next())
        .filter_map(|raw| raw.parse::<u64>().ok())
        .last()
}

fn expand_cross_logic_blocks(blocks: &[&[u64]]) -> Vec<u8> {
    let channel_count = blocks.len();
    assert!(channel_count > 0);
    let block_count = blocks[0].len();
    assert!(blocks.iter().all(|channel| channel.len() == block_count));

    let mut expanded = vec![0_u8; block_count * 64];
    for (channel_index, channel_blocks) in blocks.iter().enumerate() {
        for (block_index, block) in channel_blocks.iter().enumerate() {
            for bit_index in 0..64 {
                if ((block >> bit_index) & 1) != 0 {
                    expanded[block_index * 64 + bit_index] |= 1 << channel_index;
                }
            }
        }
    }

    expanded
}

fn pack_cross_logic_blocks(blocks: &[&[u64]]) -> Vec<u8> {
    let channel_count = blocks.len();
    assert!(channel_count > 0);
    let block_count = blocks[0].len();
    assert!(blocks.iter().all(|channel| channel.len() == block_count));

    let mut packed = Vec::with_capacity(block_count * channel_count * std::mem::size_of::<u64>());
    for block_index in 0..block_count {
        for channel_blocks in blocks {
            packed.extend_from_slice(&channel_blocks[block_index].to_le_bytes());
        }
    }

    packed
}

fn started_root_decode_session() -> DecodeExecutionSession {
    let mut session =
        DecodeExecutionSession::new().expect("decode execution session should construct");
    session
        .set_samplerate_hz(1_000_000)
        .expect("decode execution session should accept samplerate");
    session
        .build_linear_stack(&i2c_root_instance(), &[])
        .expect("decode execution session should build a linear root stack");
    session
        .start()
        .expect("decode execution session should start");
    session
}

#[test]
fn upstream_header_exists_for_boundary_tests() {
    let header = upstream_header_path();
    assert!(header.ends_with("DSView/libsigrok4DSL/libsigrok.h"));
    assert!(header.exists(), "expected upstream header at {}", header.display());
}

#[test]
fn source_runtime_path_shape_matches_cfg_state() {
    if let Some(path) = source_runtime_library_path() {
        assert!(Path::new(path).is_absolute());
        assert!(path.to_string_lossy().ends_with(runtime_library_name()));
    }
}

#[test]
fn source_decode_runtime_path_shape_matches_cfg_state() {
    if let Some(path) = source_decode_runtime_library_path() {
        assert!(Path::new(path).is_absolute());
        assert!(path.to_string_lossy().ends_with(decode_runtime_library_name()));
    }
}

#[test]
fn decode_runtime_reports_loader_failures() {
    let missing = std::env::temp_dir().join(format!(
        "missing-{}-{}",
        decode_runtime_library_name(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let error = DecodeRuntimeBridge::load(&missing).unwrap_err();

    assert!(matches!(
        error,
        DecodeRuntimeError::LibraryLoad { path, .. } if path == missing
    ));
}

#[test]
fn decode_runtime_init_rejects_missing_decoder_dir() {
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_decode_runtime() else {
        return;
    };
    let missing = std::env::temp_dir().join(format!(
        "missing-decoders-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let error = runtime.init(&missing).unwrap_err();

    assert!(matches!(
        error,
        DecodeRuntimeError::NativeCall {
            operation: "decode runtime init",
            code: DecodeRuntimeErrorCode::DecoderDirectory,
            ..
        }
    ));
}

#[test]
fn decode_runtime_success_lifecycle_contracts_hold() {
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_decode_runtime() else {
        return;
    };
    let decoder_dir = source_decoder_dir();
    if !decoder_dir.exists() {
        return;
    }

    decode_diag("success test: init runtime");
    runtime
        .init(&decoder_dir)
        .expect("decode runtime should initialize with source decoder dir");

    decode_diag("success test: decode_list");
    let list = runtime.decode_list().expect("decode_list should succeed");
    decode_diag(format!("success test: decode_list count={}", list.len()));

    let list_entry = list
        .iter()
        .find(|decoder| decoder.id == "0:i2c")
        .expect("expected canonical upstream decoder ids in decode runtime list");

    decode_diag("success test: decode_inspect 0:i2c");
    let decoder = runtime
        .decode_inspect("0:i2c")
        .expect("decode_inspect should return decoder metadata");
    assert_eq!(decoder.id, "0:i2c");
    assert_eq!(decoder.id, list_entry.id, "preserve upstream id across list/inspect");
    assert_eq!(
        decoder.longname, list_entry.longname,
        "decode_inspect should preserve upstream labels from decode_list"
    );
    assert!(!decoder.required_channels.is_empty());
    assert!(!decoder.annotation_rows.is_empty());
    assert!(!decoder.inputs.is_empty());
    assert!(!decoder.outputs.is_empty());

    decode_diag("success test: decode_inspect missing-decoder");
    let unknown = runtime.decode_inspect("missing-decoder").unwrap_err();
    assert!(matches!(
        unknown,
        DecodeRuntimeError::NativeCall {
            operation: "decode inspect",
            code: DecodeRuntimeErrorCode::UnknownDecoder,
            ..
        }
    ));

    let i2c = runtime
        .decode_inspect("0:i2c")
        .expect("i2c metadata should load");
    let address_format = i2c
        .options
        .iter()
        .find(|option| option.id == "address_format")
        .expect("i2c should expose address_format");
    assert_eq!(address_format.id, "address_format");
    assert_eq!(address_format.value_kind, DecodeOptionValueKind::String);

    let integer_decoder_id = list
        .iter()
        .find_map(|decoder| {
            decode_diag(format!("success test: inspect candidate {}", decoder.id));
            let inspected = runtime.decode_inspect(&decoder.id).ok()?;
            inspected
                .options
                .iter()
                .any(|option| option.id == "coilfreq")
                .then_some(inspected.id)
        })
        .expect("expected at least one integer option fixture");
    let integer_decoder = runtime
        .decode_inspect(&integer_decoder_id)
        .expect("integer fixture metadata should load");
    let integer_option = integer_decoder
        .options
        .iter()
        .find(|option| option.id == "coilfreq")
        .expect("integer fixture should expose coilfreq");
    assert_eq!(integer_option.id, "coilfreq");
    assert_eq!(integer_option.value_kind, DecodeOptionValueKind::Integer);

    let float_decoder_id = list
        .iter()
        .find_map(|decoder| {
            decode_diag(format!("success test: inspect float candidate {}", decoder.id));
            let inspected = runtime.decode_inspect(&decoder.id).ok()?;
            inspected
                .options
                .iter()
                .any(|option| option.id == "sample_point")
                .then_some(inspected.id)
        })
        .expect("expected at least one float option fixture");
    let float_decoder = runtime
        .decode_inspect(&float_decoder_id)
        .expect("float fixture metadata should load");
    let float_option = float_decoder
        .options
        .iter()
        .find(|option| option.id == "sample_point")
        .expect("float fixture should expose sample_point");
    assert_eq!(float_option.id, "sample_point");
    assert_eq!(float_option.value_kind, DecodeOptionValueKind::Float);

    let mut session = started_root_decode_session();
    let error = session_send_logic_chunk(
        &mut session,
        0,
        &[],
        DecodeExecutionLogicFormat::SplitLogic { unitsize: 1 },
        None,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        DecodeRuntimeError::InvalidArgument(detail) if detail.contains("sample bytes must not be empty")
    ));
    drop(session);

    let mut session = started_root_decode_session();
    let sample_bytes = [0b00, 0b01, 0b10];
    let packet_lengths = [2_usize, 1_usize];
    let error = session_send_logic_chunk(
        &mut session,
        0,
        &sample_bytes,
        DecodeExecutionLogicFormat::SplitLogic { unitsize: 2 },
        Some(&packet_lengths),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        DecodeRuntimeError::InvalidArgument(detail)
            if detail.contains("aligned to the requested logic format")
    ));
    drop(session);

    let mut session = started_root_decode_session();
    let first = [0b00, 0b01];
    session_send_logic_chunk(
        &mut session,
        0,
        &first,
        DecodeExecutionLogicFormat::SplitLogic { unitsize: 1 },
        None,
    )
    .expect("first chunk should establish the absolute cursor");
    let second = [0b10, 0b11];
    let error = session_send_logic_chunk(
        &mut session,
        0,
        &second,
        DecodeExecutionLogicFormat::SplitLogic { unitsize: 1 },
        None,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        DecodeRuntimeError::InvalidArgument(detail)
            if detail.contains("absolute sample progression")
    ));
    drop(session);

    let mut session =
        DecodeExecutionSession::new().expect("decode execution session should construct");
    session
        .set_samplerate_hz(1_000_000)
        .expect("decode execution session should accept samplerate");
    session
        .build_linear_stack(&i2c_root_instance(), &[eeprom_stack_instance()])
        .expect("decode execution session should build a stacked EEPROM chain");
    session
        .start()
        .expect("decode execution session should start");

    let samples = eeprom_byte_write_trace();
    session_send_logic_chunk(
        &mut session,
        0,
        &samples,
        DecodeExecutionLogicFormat::SplitLogic { unitsize: 1 },
        None,
    )
    .expect("stacked session should accept an EEPROM write trace");
    session
        .end()
        .expect("decode execution session should end cleanly");

    let annotations = session
        .take_captured_annotations()
        .expect("decode execution session should drain captured annotations");
    let forwarded = annotations
        .iter()
        .filter(|annotation| {
            annotation.decoder_id == "1:i2c" && annotation.annotation_class == -1
        })
        .collect::<Vec<_>>();
    assert!(
        !forwarded.is_empty()
            && forwarded
                .windows(2)
                .all(|pair| pair[0].start_sample <= pair[1].start_sample),
        "expected ordered OUTPUT_PYTHON callback traffic while a stacked decoder chain is active"
    );
    drop(session);

    decode_diag("success test: exit runtime");
    runtime.exit().expect("decode runtime exit should succeed");
    decode_diag("success test: runtime exited");
}

#[test]
fn safe_decode_decoder_wrapper_preserves_fixture_ids_and_labels() {
    let decoder = DecodeDecoder {
        id: "0:i2c".to_string(),
        name: "i2c".to_string(),
        longname: "Inter-Integrated Circuit".to_string(),
        description: "fixture decoder".to_string(),
        license: "gplv2+".to_string(),
        inputs: vec![],
        outputs: vec![],
        tags: vec!["serial".to_string()],
        required_channels: vec![],
        optional_channels: vec![],
        options: vec![],
        annotations: vec![],
        annotation_rows: vec![],
    };

    assert_eq!(decoder.id, "0:i2c", "preserve upstream id in safe wrapper");
    assert_eq!(
        decoder.longname, "Inter-Integrated Circuit",
        "preserve upstream label in safe wrapper"
    );
}

#[test]
fn synthetic_vcd_goldens_match_split_packet_fixture() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };
    let request = VcdExportRequest {
        samplerate_hz: 100_000_000,
        enabled_channels: vec![0, 1],
    };
    let sample_bytes = [0b00, 0b01, 0b10, 0b00];
    let packet_lengths = [2_usize, 2_usize];

    let export = runtime
        .render_vcd_from_logic_packets(&request, &sample_bytes, &packet_lengths, 1)
        .expect("split-packet replay should export VCD");
    let actual = normalize_vcd(&vcd_string(&export.bytes));
    let expected = fs::read_to_string(golden_path("two_channel_split_packets.vcd"))
        .expect("golden VCD should exist");

    assert_eq!(export.sample_count, 4);
    assert_eq!(export.packet_count, 4);
    assert_eq!(actual, expected);
}

#[test]
fn synthetic_vcd_goldens_lock_initial_values_deltas_and_final_timestamp() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };
    let request = VcdExportRequest {
        samplerate_hz: 250_000_000,
        enabled_channels: vec![3],
    };
    let sample_bytes = [0b0000, 0b1000, 0b0000, 0b1000];

    let export = runtime
        .render_vcd_from_samples(&request, &sample_bytes, 1)
        .expect("single-channel replay should export VCD");
    let actual = normalize_vcd(&vcd_string(&export.bytes));
    let expected = fs::read_to_string(golden_path("single_channel_final_timestamp.vcd"))
        .expect("golden VCD should exist");

    assert_eq!(export.sample_count, 4);
    assert_eq!(export.packet_count, 3);
    assert_eq!(actual, expected);
}

#[test]
fn synthetic_vcd_goldens_verify_transition_times_and_sample_count_semantics() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };
    let request = VcdExportRequest {
        samplerate_hz: 100_000_000,
        enabled_channels: vec![0, 1],
    };
    let sample_bytes = [0b00, 0b01, 0b10, 0b00];
    let packet_lengths = [2_usize, 2_usize];

    let export = runtime
        .render_vcd_from_logic_packets(&request, &sample_bytes, &packet_lengths, 1)
        .expect("split-packet replay should export VCD");
    let transitions = parse_transitions(&vcd_string(&export.bytes));

    assert_eq!(export.sample_count, sample_bytes.len() as u64);
    assert_eq!(
        transitions,
        vec![
            (0, "0!".to_string()),
            (0, "0\"".to_string()),
            (10, "1!".to_string()),
            (20, "0!".to_string()),
            (20, "1\"".to_string()),
            (30, "0\"".to_string()),
        ]
    );
    assert_eq!(final_timestamp(&vcd_string(&export.bytes)), Some(40));
}

#[test]
fn cross_logic_packet_replay_matches_expanded_sample_export() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };

    let request = VcdExportRequest {
        samplerate_hz: 8_000_000,
        enabled_channels: vec![0, 1],
    };
    let ch0 = [0x0000_FFFF_0000_FFFF_u64, 0xF0F0_F0F0_0F0F_0F0F_u64];
    let ch1 = [0xAAAA_AAAA_5555_5555_u64, 0xCCCC_3333_CCCC_3333_u64];
    let cross_bytes = pack_cross_logic_blocks(&[&ch0, &ch1]);
    let expanded_samples = expand_cross_logic_blocks(&[&ch0, &ch1]);
    let packet_lengths = [16_usize, 16_usize];

    let cross_export = runtime
        .render_vcd_from_cross_logic_packets(&request, &cross_bytes, &packet_lengths)
        .expect("cross-logic replay should export VCD");
    let expanded_export = runtime
        .render_vcd_from_samples(&request, &expanded_samples, 1)
        .expect("expanded sample replay should export VCD");

    assert_eq!(cross_export.sample_count, 128);
    assert_eq!(expanded_export.sample_count, 128);
    assert_eq!(cross_export.packet_count, 4);
    assert_eq!(
        normalize_vcd(&vcd_string(&cross_export.bytes)),
        normalize_vcd(&vcd_string(&expanded_export.bytes))
    );
}

#[test]
fn synthetic_vcd_goldens_exercise_cleanup_safe_write_contract() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };
    let request = VcdExportRequest {
        samplerate_hz: 250_000_000,
        enabled_channels: vec![3],
    };
    let sample_bytes = [0b0000, 0b1000, 0b0000, 0b1000];
    let final_path = temp_path("cleanup-safe");
    let sibling_temp = final_path.with_extension("vcd.tmp");
    let _ = fs::remove_file(&final_path);
    let _ = fs::remove_file(&sibling_temp);

    let facts = runtime
        .render_vcd_from_samples_to_path(&request, &sample_bytes, 1, &final_path)
        .expect("render-to-path should succeed");

    assert!(final_path.exists());
    assert!(!sibling_temp.exists());
    assert_eq!(facts.sample_count, 4);
    assert_eq!(facts.packet_count, 3);
    assert_eq!(
        normalize_vcd(&fs::read_to_string(&final_path).expect("final VCD should exist")),
        fs::read_to_string(golden_path("single_channel_final_timestamp.vcd"))
            .expect("golden VCD should exist")
    );

    fs::remove_file(&final_path).expect("temp VCD cleanup should succeed");
}

#[test]
fn synthetic_vcd_goldens_reject_overflow_aligned_logic_packet_replay() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };
    let request = VcdExportRequest {
        samplerate_hz: 100_000_000,
        enabled_channels: vec![0, 1],
    };
    let sample_bytes = [0b00, 0b01, 0b10, 0b00];
    let invalid_packet_lengths = [3_usize, 1_usize];
    let final_path = temp_path("overflow-precondition");
    let sibling_temp = final_path.with_extension("vcd.tmp");
    let _ = fs::remove_file(&final_path);
    let _ = fs::remove_file(&sibling_temp);

    let error = runtime
        .render_vcd_from_logic_packets_to_path(
            &request,
            &sample_bytes,
            &invalid_packet_lengths,
            2,
            &final_path,
        )
        .expect_err("misaligned packet replay should fail fast");

    assert!(matches!(error, RuntimeError::InvalidArgument(detail) if detail.contains("aligned to unitsize")));
    assert!(!final_path.exists());
    assert!(!sibling_temp.exists());
}

#[test]
fn overflow_error_code_stays_stable_for_fail_fast_contract() {
    let error = RuntimeError::ExportCall {
        operation: "ds_export_recorded_vcd",
        code: ExportErrorCode::Overflow,
    };

    match error {
        RuntimeError::ExportCall { operation, code } => {
            assert_eq!(operation, "ds_export_recorded_vcd");
            assert_eq!(code, ExportErrorCode::Overflow);
            assert_eq!(code.name(), "export_overflow");
        }
        other => panic!("expected export overflow call error, got {other:?}"),
    }
}

#[test]
fn temp_promotion_failures_reference_temp_and_final_vcd_paths() {
    let error = RuntimeError::TempPromote {
        from: Path::new("/tmp/capture.vcd.tmp").to_path_buf(),
        to: Path::new("/tmp/capture.vcd").to_path_buf(),
        detail: "cross-device rename".to_string(),
    };

    match error {
        RuntimeError::TempPromote { from, to, detail } => {
            assert!(from.ends_with("capture.vcd.tmp"));
            assert!(to.ends_with("capture.vcd"));
            assert_eq!(detail, "cross-device rename");
        }
        other => panic!("expected temp promote failure, got {other:?}"),
    }
}

struct VirtualSessionGuard<'a> {
    runtime: &'a RuntimeBridge,
    handle: Option<DeviceHandle>,
    opened: bool,
    callbacks: bool,
    initialized: bool,
}

impl<'a> VirtualSessionGuard<'a> {
    fn new(runtime: &'a RuntimeBridge) -> Self {
        Self {
            runtime,
            handle: None,
            opened: false,
            callbacks: false,
            initialized: false,
        }
    }
}

impl Drop for VirtualSessionGuard<'_> {
    fn drop(&mut self) {
        if self.callbacks {
            let _ = self.runtime.clear_acquisition_callbacks();
        }
        if self.opened {
            let _ = self.runtime.release_device();
        }
        if let Some(handle) = self.handle {
            let _ = self.runtime.remove_device(handle);
        }
        if self.initialized {
            let _ = self.runtime.exit();
        }
    }
}

fn read_zip_entry_names(zip_bytes: &[u8]) -> Vec<String> {
    let eocd_sig = [0x50_u8, 0x4b, 0x05, 0x06];
    let eocd_offset = zip_bytes
        .windows(4)
        .rposition(|window| window == eocd_sig)
        .expect("ZIP archive should contain End of Central Directory record");
    let cd_size = u32::from_le_bytes(zip_bytes[eocd_offset + 12..eocd_offset + 16].try_into().unwrap()) as usize;
    let cd_offset = u32::from_le_bytes(zip_bytes[eocd_offset + 16..eocd_offset + 20].try_into().unwrap()) as usize;
    let total_entries = u16::from_le_bytes(zip_bytes[eocd_offset + 10..eocd_offset + 12].try_into().unwrap()) as usize;

    let mut pos = cd_offset;
    let cd_end = cd_offset + cd_size;
    let mut names = Vec::with_capacity(total_entries);
    while pos < cd_end {
        assert_eq!(&zip_bytes[pos..pos + 4], &[0x50, 0x4b, 0x01, 0x02]);
        let name_len = u16::from_le_bytes(zip_bytes[pos + 28..pos + 30].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(zip_bytes[pos + 30..pos + 32].try_into().unwrap()) as usize;
        let comment_len = u16::from_le_bytes(zip_bytes[pos + 32..pos + 34].try_into().unwrap()) as usize;
        let name_start = pos + 46;
        let name = String::from_utf8(zip_bytes[name_start..name_start + name_len].to_vec())
            .expect("ZIP entry name should be UTF-8");
        names.push(name);
        pos = name_start + name_len + extra_len + comment_len;
    }
    assert_eq!(names.len(), total_entries);
    names
}

fn replay_dsl_through_virtual_session(
    runtime: &RuntimeBridge,
    dsl_path: &Path,
    enabled_channels: Vec<u16>,
) -> (u64, u64, u64, String) {
    let mut guard = VirtualSessionGuard::new(runtime);
    runtime.init().expect("runtime init should succeed");
    guard.initialized = true;

    runtime
        .device_from_file(dsl_path)
        .expect("ds_device_from_file should load generated .dsl");

    let file_stem = dsl_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("dsl path should have filename");
    let devices = runtime.list_devices().expect("device list should succeed");
    let dev = devices
        .iter()
        .find(|device| device.name.contains(file_stem) || device.name.contains(".dsl"))
        .unwrap_or_else(|| {
            panic!("expected virtual-session device for {file_stem}, got {devices:?}")
        });
    guard.handle = Some(dev.handle);

    runtime.open_device(dev.handle).expect("opening virtual-session should succeed");
    guard.opened = true;

    let current_samplerate_hz = runtime
        .current_samplerate()
        .expect("virtual-session current_samplerate should read cleanly")
        .expect("virtual-session current_samplerate should be present");
    let current_sample_limit = runtime
        .current_sample_limit()
        .expect("virtual-session current_sample_limit should read cleanly")
        .expect("virtual-session current_sample_limit should be present");

    runtime
        .reset_acquisition_summary()
        .expect("reset summary should succeed");
    runtime
        .register_acquisition_callbacks()
        .expect("register acquisition callbacks should succeed");
    guard.callbacks = true;
    runtime
        .start_collect()
        .expect("virtual-session start_collect should succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let state = runtime
            .acquisition_state()
            .expect("acquisition state should succeed");
        if !state.is_collecting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "virtual-session replay timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let recorded_samples = runtime
        .recorded_stream_sample_count()
        .expect("recorded_stream_sample_count should succeed");
    let vcd = runtime
        .export_recorded_vcd(&VcdExportRequest {
            samplerate_hz: current_samplerate_hz,
            enabled_channels,
        })
        .expect("export_recorded_vcd after virtual-session replay should succeed");
    assert_eq!(vcd.sample_count, recorded_samples);

    (
        current_samplerate_hz,
        current_sample_limit,
        recorded_samples,
        normalize_vcd(&vcd_string(&vcd.bytes)),
    )
}

#[test]
fn dsl_cross_logic_packet_export_and_virtual_session_roundtrip() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };

    let ch0 = [0x0000_FFFF_0000_FFFF_u64, 0xF0F0_F0F0_0F0F_0F0F_u64];
    let ch1 = [0xAAAA_AAAA_5555_5555_u64, 0xCCCC_3333_CCCC_3333_u64];
    let ch2 = [0x1234_5678_9ABC_DEF0_u64, 0x0FED_CBA9_8765_4321_u64];
    let cross_bytes = pack_cross_logic_blocks(&[&ch0, &ch1, &ch2]);
    let packet_lengths = [24_usize, 24_usize];

    let dsl_path = temp_path("cross-roundtrip").with_extension("dsl");
    let _ = fs::remove_file(&dsl_path);

    let dsl_request = DslExportRequest {
        samplerate_hz: 8_000_000,
        channels: vec![
            DslChannelSpec {
                index: 0,
                name: "CLK".to_string(),
            },
            DslChannelSpec {
                index: 2,
                name: "MOSI".to_string(),
            },
            DslChannelSpec {
                index: 5,
                name: "CS#".to_string(),
            },
        ],
        driver_name: "DSLogic Plus".to_string(),
        trigger_time_ms: 1731552000123,
        trigger_pos: 17,
        session_json: "{\"Version\":3,\"Device\":\"DSLogic Plus\",\"DeviceMode\":0,\"Sample count\":\"128\",\"Sample rate\":\"8000000\",\"Operation Mode\":\"Buffer Mode\",\"decoder\":[],\"channel\":[]}".to_string(),
        decoders_json: "[]".to_string(),
    };

    let facts = runtime
        .render_dsl_from_cross_logic_packets_to_path(
            &dsl_request,
            &cross_bytes,
            &packet_lengths,
            &dsl_path,
        )
        .expect("cross-logic .dsl export should succeed");
    assert_eq!(facts.sample_count, 128);
    assert_eq!(facts.packet_count, 4);
    assert!(facts.output_bytes > 0);
    assert!(dsl_path.exists());
    assert!(!dsl_path.with_extension("dsl.tmp").exists());

    let zip_bytes = fs::read(&dsl_path).expect(".dsl file should be readable");
    let entry_names = read_zip_entry_names(&zip_bytes);
    assert_eq!(
        entry_names,
        vec![
            "L-0/0".to_string(),
            "L-2/0".to_string(),
            "L-5/0".to_string(),
            "header".to_string(),
            "decoders".to_string(),
            "session".to_string(),
        ]
    );

    let expected_vcd = runtime
        .render_vcd_from_cross_logic_packets(
            &VcdExportRequest {
                samplerate_hz: 8_000_000,
                enabled_channels: vec![0, 2, 5],
            },
            &cross_bytes,
            &packet_lengths,
        )
        .expect("direct VCD render should succeed");

    let (current_samplerate_hz, current_sample_limit, replayed_samples, replayed_vcd) =
        replay_dsl_through_virtual_session(&runtime, &dsl_path, vec![0, 2, 5]);
    assert_eq!(current_samplerate_hz, 8_000_000);
    assert_eq!(current_sample_limit, 128);
    assert_eq!(replayed_samples, 128);
    assert_eq!(replayed_vcd, normalize_vcd(&vcd_string(&expected_vcd.bytes)));

    let _ = fs::remove_file(&dsl_path);
}

#[test]
fn dsl_multi_block_split_export_roundtrips_above_16m_samples_and_non_metric_rate() {
    if skip_windows_vcd_goldens() {
        return;
    }
    let _guard = runtime_test_guard().lock().unwrap();
    let Some(runtime) = load_runtime() else {
        return;
    };

    const ONE_BLOCK_SAMPLES: usize = 16_777_216;
    const TOTAL_SAMPLES: usize = ONE_BLOCK_SAMPLES + 128;
    let mut sample_bytes = vec![0_u8; TOTAL_SAMPLES];
    sample_bytes[1] = 1;
    sample_bytes[ONE_BLOCK_SAMPLES] = 1;
    sample_bytes[TOTAL_SAMPLES - 1] = 1;
    let packet_lengths = [ONE_BLOCK_SAMPLES, 128_usize];

    let dsl_path = temp_path("multiblock").with_extension("dsl");
    let _ = fs::remove_file(&dsl_path);

    let dsl_request = DslExportRequest {
        samplerate_hz: 125_500,
        channels: vec![DslChannelSpec {
            index: 0,
            name: "D0".to_string(),
        }],
        driver_name: "DSLogic Plus".to_string(),
        trigger_time_ms: 0,
        trigger_pos: 0,
        session_json: "{\"Version\":3}".to_string(),
        decoders_json: "[]".to_string(),
    };

    let facts = runtime
        .render_dsl_from_logic_packets_to_path(
            &dsl_request,
            &sample_bytes,
            &packet_lengths,
            1,
            &dsl_path,
        )
        .expect("multi-block .dsl export should succeed");
    assert_eq!(facts.sample_count, TOTAL_SAMPLES as u64);

    let zip_bytes = fs::read(&dsl_path).expect(".dsl file should be readable");
    let entry_names = read_zip_entry_names(&zip_bytes);
    assert_eq!(
        entry_names,
        vec![
            "L-0/0".to_string(),
            "L-0/1".to_string(),
            "header".to_string(),
            "decoders".to_string(),
            "session".to_string(),
        ]
    );

    let (current_samplerate_hz, current_sample_limit, replayed_samples, _replayed_vcd) =
        replay_dsl_through_virtual_session(&runtime, &dsl_path, vec![0]);
    assert_eq!(current_samplerate_hz, 125_500);
    assert_eq!(current_sample_limit, TOTAL_SAMPLES as u64);
    assert_eq!(replayed_samples, TOTAL_SAMPLES as u64);

    let _ = fs::remove_file(&dsl_path);
}
