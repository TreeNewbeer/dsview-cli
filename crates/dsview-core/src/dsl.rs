//! DSView `.dsl` session archive planning, session JSON formatting, and test-fixture writing.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dsview_sys::{DslChannelSpec, DslExportRequest};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::device_options::DeviceOptionsSnapshot;
use crate::trigger::{
    CaptureTriggerOutcome, TriggerChannelConditions, TriggerCombine, TriggerCondition,
    TriggerPattern, TriggerSpec, TRIGGER_MAX_STAGES, TRIGGER_POSITION_DEFAULT_PERCENT,
};
use crate::{CaptureDeviceOptionSnapshot, metadata_path_for_vcd};

const CHANNEL_TYPE_LOGIC: u32 = 10_000;
const TRIGGER_PATTERN_WIDTH: usize = 16;

/// Resolved capture output file targets supporting `.vcd`, `.dsl`, or both formats together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutputPlan {
    pub vcd_path: Option<PathBuf>,
    pub dsl_path: Option<PathBuf>,
    pub metadata_path: PathBuf,
}

/// Errors reported when validating or resolving `.vcd`, `.dsl`, and metadata output paths.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureOutputPlanError {
    #[error("either an output path (.vcd or .dsl) or --dsl-output is required")]
    MissingPrimaryOutput,
    #[error("output path `{path}` must use the .vcd or .dsl extension")]
    InvalidOutputExtension { path: PathBuf },
    #[error("DSL output path `{path}` must use the .dsl extension")]
    InvalidDslExtension { path: PathBuf },
    #[error("metadata output path `{path}` must use the .json extension")]
    InvalidMetadataExtension { path: PathBuf },
    #[error(
        "cannot specify `--dsl-output` (`{dsl_output_path}`) when `--output` (`{output_path}`) already ends with .dsl"
    )]
    DuplicateDslTarget {
        output_path: PathBuf,
        dsl_output_path: PathBuf,
    },
    #[error("artifact path `{left}` and `{right}` must be different")]
    ConflictingArtifactPaths { left: PathBuf, right: PathBuf },
}

/// Resolve and validate capture output paths from `--output`, `--dsl-output`, and `--metadata-output`.
pub fn resolve_capture_output_plan(
    output_path: Option<impl AsRef<Path>>,
    dsl_output_path: Option<impl AsRef<Path>>,
    metadata_path: Option<impl AsRef<Path>>,
) -> Result<CaptureOutputPlan, CaptureOutputPlanError> {
    let output_path = output_path.map(|path| path.as_ref().to_path_buf());
    let explicit_dsl = dsl_output_path.map(|path| path.as_ref().to_path_buf());

    if let Some(dsl_path) = explicit_dsl.as_ref() {
        if path_extension_str(dsl_path) != Some("dsl") {
            return Err(CaptureOutputPlanError::InvalidDslExtension {
                path: dsl_path.clone(),
            });
        }
    }

    let (vcd_path, dsl_path, primary_for_metadata) = match (output_path, explicit_dsl) {
        (None, None) => return Err(CaptureOutputPlanError::MissingPrimaryOutput),
        (None, Some(dsl_path)) => (None, Some(dsl_path.clone()), dsl_path),
        (Some(primary_path), extra_dsl) => match path_extension_str(&primary_path) {
            Some("vcd") => (Some(primary_path.clone()), extra_dsl, primary_path),
            Some("dsl") => {
                if let Some(dsl_output_path) = extra_dsl {
                    return Err(CaptureOutputPlanError::DuplicateDslTarget {
                        output_path: primary_path,
                        dsl_output_path,
                    });
                }
                (None, Some(primary_path.clone()), primary_path)
            }
            _ => {
                return Err(CaptureOutputPlanError::InvalidOutputExtension {
                    path: primary_path,
                });
            }
        },
    };

    let metadata_path = metadata_path
        .map(|path| path.as_ref().to_path_buf())
        .unwrap_or_else(|| metadata_path_for_vcd(&primary_for_metadata));

    if let Some(vcd) = vcd_path.as_ref() {
        if vcd == &metadata_path {
            return Err(CaptureOutputPlanError::ConflictingArtifactPaths {
                left: vcd.clone(),
                right: metadata_path,
            });
        }
        if let Some(dsl) = dsl_path.as_ref() {
            if vcd == dsl {
                return Err(CaptureOutputPlanError::ConflictingArtifactPaths {
                    left: vcd.clone(),
                    right: dsl.clone(),
                });
            }
        }
    }
    if let Some(dsl) = dsl_path.as_ref() {
        if dsl == &metadata_path {
            return Err(CaptureOutputPlanError::ConflictingArtifactPaths {
                left: dsl.clone(),
                right: metadata_path,
            });
        }
    }

    if path_extension_str(&metadata_path) != Some("json") {
        return Err(CaptureOutputPlanError::InvalidMetadataExtension {
            path: metadata_path,
        });
    }

    Ok(CaptureOutputPlan {
        vcd_path,
        dsl_path,
        metadata_path,
    })
}

fn path_extension_str(path: &Path) -> Option<&str> {
    path.extension().and_then(|ext| ext.to_str())
}

/// Inputs used to construct a DSView `.dsl` export request matching the captured hardware state.
#[derive(Debug, Clone)]
pub struct DslSessionMetadataInput<'a> {
    pub title: &'a str,
    pub device_model: &'a str,
    pub samplerate_hz: u64,
    pub sample_count: u64,
    pub enabled_channels: &'a [u16],
    pub capture_started_at: SystemTime,
    pub device_options_snapshot: &'a DeviceOptionsSnapshot,
    pub effective_options: &'a CaptureDeviceOptionSnapshot,
    pub trigger: Option<&'a CaptureTriggerOutcome>,
}

/// Construct a [`DslExportRequest`] containing DSView v3 `session` JSON and channel specs.
pub fn build_dsl_export_request(input: &DslSessionMetadataInput<'_>) -> DslExportRequest {
    let trigger_time_ms = system_time_unix_millis(input.capture_started_at);
    let trigger_pos = input
        .trigger
        .filter(|outcome| outcome.observation.fired())
        .and_then(|outcome| outcome.observation.position)
        .map(|position| u64::from(position.real_pos))
        .unwrap_or(0);

    let channels = input
        .enabled_channels
        .iter()
        .map(|&index| DslChannelSpec {
            index,
            name: index.to_string(),
        })
        .collect();

    let session_json = build_dsl_session_json(input);

    DslExportRequest {
        samplerate_hz: input.samplerate_hz,
        channels,
        driver_name: input.device_model.to_string(),
        trigger_time_ms,
        trigger_pos,
        session_json,
        decoders_json: "[]".to_string(),
    }
}

fn system_time_unix_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => {
            let millis = i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX);
            millis.saturating_neg()
        }
    }
}

fn build_dsl_session_json(input: &DslSessionMetadataInput<'_>) -> String {
    let mut root = Map::new();
    root.insert("Device".to_string(), Value::String(input.device_model.to_string()));
    root.insert("DeviceMode".to_string(), Value::from(0));
    root.insert("Max Height".to_string(), Value::String("1X".to_string()));
    root.insert(
        "Sample count".to_string(),
        Value::String(input.sample_count.to_string()),
    );
    root.insert(
        "Sample rate".to_string(),
        Value::String(input.samplerate_hz.to_string()),
    );
    root.insert("Title".to_string(), Value::String(input.title.to_string()));
    root.insert("Version".to_string(), Value::from(3));

    if let Some(op_mode) = lookup_operation_mode_label(
        input.device_options_snapshot,
        &input.effective_options.operation_mode_id,
    ) {
        root.insert("Operation Mode".to_string(), Value::String(op_mode));
    }
    if let Some(ch_mode) = lookup_channel_mode_label(
        input.device_options_snapshot,
        &input.effective_options.operation_mode_id,
        &input.effective_options.channel_mode_id,
    ) {
        root.insert("Channel Mode".to_string(), Value::String(ch_mode));
    }
    if let Some(stop_option_id) = input.effective_options.stop_option_id.as_deref() {
        if let Some(label) = lookup_enum_label(&input.device_options_snapshot.stop_options, stop_option_id) {
            root.insert("Stop Options".to_string(), Value::String(label));
        }
    }
    if let Some(filter_id) = input.effective_options.filter_id.as_deref() {
        if let Some(label) = lookup_enum_label(&input.device_options_snapshot.filters, filter_id) {
            root.insert("Filter Targets".to_string(), Value::String(label));
        }
    }
    if let Some(threshold_volts) = input.effective_options.threshold_volts {
        root.insert(
            "Threshold Level".to_string(),
            Value::String(format_threshold_volts(threshold_volts)),
        );
    }

    let simple_triggers = simple_trigger_conditions(input.trigger);
    let channels_json: Vec<Value> = input
        .enabled_channels
        .iter()
        .enumerate()
        .map(|(view_index, &hw_index)| {
            let strigger = simple_triggers
                .and_then(|conditions| conditions.get(&hw_index))
                .copied()
                .map(logic_set_region_code)
                .unwrap_or(0);
            json!({
                "colour": "default",
                "enabled": true,
                "index": hw_index,
                "name": hw_index.to_string(),
                "strigger": strigger,
                "type": CHANNEL_TYPE_LOGIC,
                "view_index": view_index,
            })
        })
        .collect();
    root.insert("channel".to_string(), Value::Array(channels_json));
    root.insert("decoder".to_string(), Value::Array(Vec::new()));
    root.insert("trigger".to_string(), build_dsl_trigger_json(input.trigger));

    serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string())
}

fn lookup_operation_mode_label(snapshot: &DeviceOptionsSnapshot, id: &str) -> Option<String> {
    lookup_enum_label(&snapshot.operation_modes, id).or_else(|| {
        match id {
            "buffer" => Some("Buffer Mode".to_string()),
            "stream" => Some("Stream Mode".to_string()),
            _ => None,
        }
    })
}

fn lookup_channel_mode_label(
    snapshot: &DeviceOptionsSnapshot,
    operation_mode_id: &str,
    channel_mode_id: &str,
) -> Option<String> {
    snapshot
        .channel_modes_by_operation_mode
        .iter()
        .find(|group| group.operation_mode_id == operation_mode_id)
        .and_then(|group| {
            group
                .channel_modes
                .iter()
                .find(|mode| mode.id == channel_mode_id)
                .map(|mode| mode.label.clone())
        })
}

fn lookup_enum_label(
    options: &[crate::device_options::EnumOptionSnapshot],
    id: &str,
) -> Option<String> {
    options
        .iter()
        .find(|option| option.id == id)
        .map(|option| option.label.clone())
}

fn format_threshold_volts(volts: f64) -> String {
    let rounded = (volts * 10.0).round() / 10.0;
    if (volts - rounded).abs() < 1e-6 {
        format!("{rounded:.1}")
    } else {
        format!("{volts:.2}")
    }
}

fn simple_trigger_conditions(
    trigger: Option<&CaptureTriggerOutcome>,
) -> Option<&TriggerChannelConditions> {
    match &trigger?.requested.spec {
        TriggerSpec::Simple { channels } => Some(channels),
        TriggerSpec::Stage { .. } | TriggerSpec::Serial(_) => None,
    }
}

fn logic_set_region_code(condition: TriggerCondition) -> u8 {
    match condition {
        TriggerCondition::Rising => 1,
        TriggerCondition::High => 2,
        TriggerCondition::Falling => 3,
        TriggerCondition::Low => 4,
        TriggerCondition::Edge => 5,
    }
}

fn build_dsl_trigger_json(trigger: Option<&CaptureTriggerOutcome>) -> Value {
    let dont_care_pattern = "X ".repeat(TRIGGER_PATTERN_WIDTH).trim_end().to_string();
    let mut obj = Map::new();

    let (adv_mode, trigger_tab, trigger_stages, trigger_pos) = match trigger {
        Some(outcome) => match &outcome.requested.spec {
            TriggerSpec::Simple { .. } => (false, 0_usize, 0_usize, outcome.requested.position_percent),
            TriggerSpec::Stage { stages } => (
                true,
                0_usize,
                stages.len().saturating_sub(1),
                outcome.requested.position_percent,
            ),
            TriggerSpec::Serial(_) => (true, 1_usize, 0_usize, outcome.requested.position_percent),
        },
        None => (false, 0_usize, 0_usize, TRIGGER_POSITION_DEFAULT_PERCENT),
    };

    obj.insert("advTriggerMode".to_string(), Value::Bool(adv_mode));
    obj.insert("triggerTab".to_string(), Value::from(trigger_tab));
    obj.insert("triggerStages".to_string(), Value::from(trigger_stages));
    obj.insert("triggerPos".to_string(), Value::from(trigger_pos));

    let stages = match trigger.map(|outcome| &outcome.requested.spec) {
        Some(TriggerSpec::Stage { stages }) => stages.as_slice(),
        _ => &[],
    };
    for stage_idx in 0..TRIGGER_MAX_STAGES {
        let stage = stages.get(stage_idx);
        let (val0, inv0) = stage
            .map(|spec| format_trigger_pattern(&spec.pattern_a))
            .unwrap_or_else(|| (dont_care_pattern.clone(), 0));
        let (val1, inv1) = stage
            .map(|spec| format_trigger_pattern(&spec.pattern_b))
            .unwrap_or_else(|| (dont_care_pattern.clone(), 0));
        let logic = stage
            .map(|spec| match spec.combine {
                TriggerCombine::Or => 0_u8,
                TriggerCombine::And => 1_u8,
            })
            .unwrap_or(1);
        let count = stage.map(|spec| spec.count).unwrap_or(1);
        let contiguous = stage.map(|spec| spec.contiguous).unwrap_or(false);

        obj.insert(format!("stageTriggerValue0{stage_idx}"), Value::String(val0));
        obj.insert(format!("stageTriggerValue1{stage_idx}"), Value::String(val1));
        obj.insert(format!("stageTriggerInv0{stage_idx}"), Value::from(inv0));
        obj.insert(format!("stageTriggerInv1{stage_idx}"), Value::from(inv1));
        obj.insert(format!("stageTriggerLogic{stage_idx}"), Value::from(logic));
        obj.insert(format!("stageTriggerCount{stage_idx}"), Value::from(count));
        obj.insert(
            format!("stageTriggerContiguous{stage_idx}"),
            Value::Bool(contiguous),
        );
    }

    match trigger.map(|outcome| &outcome.requested.spec) {
        Some(TriggerSpec::Serial(serial)) => {
            obj.insert(
                "serialTriggerBits".to_string(),
                Value::from(serial.data_bits.saturating_sub(1)),
            );
            obj.insert(
                "serialTriggerChannel".to_string(),
                Value::from(serial.data_channel),
            );
            obj.insert(
                "serialTriggerStart".to_string(),
                Value::String(format_conditions_pattern(&serial.start)),
            );
            obj.insert(
                "serialTriggerStop".to_string(),
                Value::String(format_conditions_pattern(&serial.stop)),
            );
            obj.insert(
                "serialTriggerClock".to_string(),
                Value::String(format_conditions_pattern(&serial.clock)),
            );
            let mut tokens = vec!["X".to_string(); TRIGGER_PATTERN_WIDTH];
            for (bit_idx, ch) in serial.data_value.chars().rev().enumerate() {
                if bit_idx < TRIGGER_PATTERN_WIDTH {
                    tokens[bit_idx] = ch.to_string();
                }
            }
            obj.insert(
                "serialTriggerData".to_string(),
                Value::String(tokens.join(" ")),
            );
        }
        _ => {
            obj.insert("serialTriggerBits".to_string(), Value::from(0));
            obj.insert("serialTriggerChannel".to_string(), Value::from(0));
            obj.insert(
                "serialTriggerClock".to_string(),
                Value::String(dont_care_pattern.clone()),
            );
            obj.insert(
                "serialTriggerData".to_string(),
                Value::String(dont_care_pattern.clone()),
            );
            obj.insert(
                "serialTriggerStart".to_string(),
                Value::String(dont_care_pattern.clone()),
            );
            obj.insert(
                "serialTriggerStop".to_string(),
                Value::String(dont_care_pattern),
            );
        }
    }

    Value::Object(obj)
}

fn format_trigger_pattern(pattern: &TriggerPattern) -> (String, u8) {
    (
        format_conditions_pattern(&pattern.channels),
        if pattern.invert { 1 } else { 0 },
    )
}

fn format_conditions_pattern(conditions: &TriggerChannelConditions) -> String {
    (0..TRIGGER_PATTERN_WIDTH)
        .map(|channel| {
            conditions
                .get(&(channel as u16))
                .map(|condition| condition.dsview_symbol().to_string())
                .unwrap_or_else(|| "X".to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format an integer sample rate using the unit tokens accepted by `sr_parse_sizestring`.
pub fn format_dsl_header_samplerate(samplerate_hz: u64) -> String {
    if samplerate_hz >= 1_000_000_000 && samplerate_hz % 1_000_000_000 == 0 {
        format!("{} GHz", samplerate_hz / 1_000_000_000)
    } else if samplerate_hz >= 1_000_000 && samplerate_hz % 1_000_000 == 0 {
        format!("{} MHz", samplerate_hz / 1_000_000)
    } else if samplerate_hz >= 1_000 && samplerate_hz % 1_000 == 0 {
        format!("{} kHz", samplerate_hz / 1_000)
    } else {
        format!("{samplerate_hz} Hz")
    }
}

/// Write a standards-compliant uncompressed (STORE) `.dsl` ZIP archive for offline test fixtures.
///
/// Each channel `L-<ch>/0` stores 8-byte-aligned bit-packed sample bytes, matching `session_driver.c`
/// version 3 without requiring `libdsview_runtime` to be loaded.
pub fn write_minimal_store_dsl_zip(
    path: impl AsRef<Path>,
    request: &DslExportRequest,
    channel_data: &BTreeMap<u16, Vec<u8>>,
    total_samples: u64,
) -> io::Result<u64> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let raw_bytes = ((total_samples as usize) + 7) / 8;
    let padded_bytes = ((raw_bytes + 7) / 8) * 8;
    let padded_bytes = padded_bytes.max(8);

    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(request.channels.len() + 3);
    for ch in &request.channels {
        let mut block = channel_data.get(&ch.index).cloned().unwrap_or_default();
        block.resize(padded_bytes, 0);
        entries.push((format!("L-{}/0", ch.index), block));
    }

    let mut header = format!(
        "[version]\nversion = 3\n[header]\ndriver = {}\ndevice mode = 0\ncapturefile = data\ntotal samples = {}\ntotal probes = {}\ntotal blocks = 1\nsamplerate = {}\ntrigger time = {}\ntrigger pos = {}\n",
        request.driver_name,
        total_samples,
        request.channels.len(),
        format_dsl_header_samplerate(request.samplerate_hz),
        request.trigger_time_ms,
        request.trigger_pos
    );
    for ch in &request.channels {
        header.push_str(&format!("probe{} = {}\n", ch.index, ch.name));
    }

    entries.push(("header".to_string(), header.into_bytes()));
    entries.push(("decoders".to_string(), request.decoders_json.as_bytes().to_vec()));
    entries.push(("session".to_string(), request.session_json.as_bytes().to_vec()));

    let zip_bytes = encode_store_zip_entries(&entries)?;
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|ext| ext.to_str()).unwrap_or("dsl")
    ));
    fs::write(&temp_path, &zip_bytes)?;
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(zip_bytes.len() as u64)
}

fn encode_store_zip_entries(entries: &[(String, Vec<u8>)]) -> io::Result<Vec<u8>> {
    struct CentralRecord {
        name: Vec<u8>,
        crc32: u32,
        size: u32,
        local_offset: u32,
    }

    let mut out = Vec::new();
    let mut records = Vec::with_capacity(entries.len());

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry name too long"))?;
        let data_size = u32::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry exceeds 4 GiB"))?;
        let local_offset = u32::try_from(out.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP offset exceeds 4 GiB"))?;
        let crc = crc32_ieee(data);

        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        out.extend_from_slice(&20_u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0_u16.to_le_bytes()); // flags
        out.extend_from_slice(&0_u16.to_le_bytes()); // compression = STORE (0)
        out.extend_from_slice(&0_u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0_u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&data_size.to_le_bytes());
        out.extend_from_slice(&data_size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        records.push(CentralRecord {
            name: name_bytes.to_vec(),
            crc32: crc,
            size: data_size,
            local_offset,
        });
    }

    let cd_offset = u32::try_from(out.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP offset exceeds 4 GiB"))?;
    for record in &records {
        let name_len = record.name.len() as u16;
        out.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        out.extend_from_slice(&20_u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20_u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0_u16.to_le_bytes()); // flags
        out.extend_from_slice(&0_u16.to_le_bytes()); // compression = STORE
        out.extend_from_slice(&0_u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0_u16.to_le_bytes()); // mod date
        out.extend_from_slice(&record.crc32.to_le_bytes());
        out.extend_from_slice(&record.size.to_le_bytes());
        out.extend_from_slice(&record.size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes()); // extra len
        out.extend_from_slice(&0_u16.to_le_bytes()); // comment len
        out.extend_from_slice(&0_u16.to_le_bytes()); // disk number
        out.extend_from_slice(&0_u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0_u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&record.local_offset.to_le_bytes());
        out.extend_from_slice(&record.name);
    }

    let cd_size = u32::try_from(out.len() - cd_offset as usize)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP directory too large"))?;
    let entry_count = u16::try_from(records.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Too many ZIP entries"))?;

    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0_u16.to_le_bytes()); // start disk
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // comment len

    Ok(out)
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_options::{
        ChannelModeGroupSnapshot, ChannelModeOptionSnapshot, CurrentDeviceOptionValues,
        DeviceIdentitySnapshot, EnumOptionSnapshot, ThresholdCapabilitySnapshot,
    };
    use crate::trigger::ValidatedTrigger;
    use dsview_sys::{
        AcquisitionPacketStatus, TRIGGER_PROBE_CAPACITY, TriggerObservation, TriggerPosition,
        TriggerProgramMode,
    };

    fn sample_snapshot() -> DeviceOptionsSnapshot {
        DeviceOptionsSnapshot {
            device: DeviceIdentitySnapshot {
                selection_handle: 1,
                native_handle: 1,
                stable_id: "usb-1".to_string(),
                kind: "DSLogic Plus".to_string(),
                name: "DSLogic Plus".to_string(),
            },
            current: CurrentDeviceOptionValues {
                operation_mode_id: Some("buffer".to_string()),
                operation_mode_code: Some(0),
                stop_option_id: Some("immediate".to_string()),
                stop_option_code: Some(0),
                filter_id: Some("none".to_string()),
                filter_code: Some(0),
                channel_mode_id: Some("16ch_100mhz".to_string()),
                channel_mode_code: Some(0),
            },
            operation_modes: vec![
                EnumOptionSnapshot {
                    id: "buffer".to_string(),
                    native_code: 0,
                    label: "Buffer Mode".to_string(),
                },
                EnumOptionSnapshot {
                    id: "stream".to_string(),
                    native_code: 1,
                    label: "Stream Mode".to_string(),
                },
            ],
            stop_options: vec![EnumOptionSnapshot {
                id: "immediate".to_string(),
                native_code: 0,
                label: "Upload captured data immediately".to_string(),
            }],
            filters: vec![EnumOptionSnapshot {
                id: "none".to_string(),
                native_code: 0,
                label: "None".to_string(),
            }],
            channel_modes_by_operation_mode: vec![ChannelModeGroupSnapshot {
                operation_mode_id: "buffer".to_string(),
                operation_mode_code: 0,
                current_channel_mode_id: Some("16ch_100mhz".to_string()),
                current_channel_mode_code: Some(0),
                channel_modes: vec![ChannelModeOptionSnapshot {
                    id: "16ch_100mhz".to_string(),
                    native_code: 0,
                    label: "Use 16 Channels (Max 100MHz)".to_string(),
                    max_enabled_channels: 16,
                }],
            }],
            threshold: ThresholdCapabilitySnapshot {
                id: "threshold_volts".to_string(),
                kind: "continuous".to_string(),
                current_volts: Some(1.5),
                min_volts: 0.0,
                max_volts: 5.0,
                step_volts: 0.1,
                legacy_metadata: None,
            },
        }
    }

    #[test]
    fn resolve_capture_output_plan_supports_vcd_dsl_and_dual_modes() {
        let vcd_only =
            resolve_capture_output_plan(Some("/tmp/run.vcd"), Option::<PathBuf>::None, Option::<PathBuf>::None)
                .unwrap();
        assert_eq!(vcd_only.vcd_path, Some(PathBuf::from("/tmp/run.vcd")));
        assert_eq!(vcd_only.dsl_path, None);
        assert_eq!(vcd_only.metadata_path, PathBuf::from("/tmp/run.json"));

        let dsl_as_output =
            resolve_capture_output_plan(Some("/tmp/run.dsl"), Option::<PathBuf>::None, Option::<PathBuf>::None)
                .unwrap();
        assert_eq!(dsl_as_output.vcd_path, None);
        assert_eq!(dsl_as_output.dsl_path, Some(PathBuf::from("/tmp/run.dsl")));
        assert_eq!(dsl_as_output.metadata_path, PathBuf::from("/tmp/run.json"));

        let dsl_flag_only =
            resolve_capture_output_plan(Option::<PathBuf>::None, Some("/tmp/wave.dsl"), Option::<PathBuf>::None)
                .unwrap();
        assert_eq!(dsl_flag_only.vcd_path, None);
        assert_eq!(dsl_flag_only.dsl_path, Some(PathBuf::from("/tmp/wave.dsl")));
        assert_eq!(dsl_flag_only.metadata_path, PathBuf::from("/tmp/wave.json"));

        let both = resolve_capture_output_plan(
            Some("/tmp/run.vcd"),
            Some("/tmp/run.dsl"),
            Some("/tmp/meta.json"),
        )
        .unwrap();
        assert_eq!(both.vcd_path, Some(PathBuf::from("/tmp/run.vcd")));
        assert_eq!(both.dsl_path, Some(PathBuf::from("/tmp/run.dsl")));
        assert_eq!(both.metadata_path, PathBuf::from("/tmp/meta.json"));
    }

    #[test]
    fn resolve_capture_output_plan_rejects_duplicate_and_bad_extensions() {
        assert!(matches!(
            resolve_capture_output_plan(
                Some("/tmp/a.dsl"),
                Some("/tmp/b.dsl"),
                Option::<PathBuf>::None,
            ),
            Err(CaptureOutputPlanError::DuplicateDslTarget { .. })
        ));
        assert!(matches!(
            resolve_capture_output_plan(
                Some("/tmp/a.txt"),
                Option::<PathBuf>::None,
                Option::<PathBuf>::None,
            ),
            Err(CaptureOutputPlanError::InvalidOutputExtension { .. })
        ));
        assert!(matches!(
            resolve_capture_output_plan(
                Some("/tmp/a.vcd"),
                Some("/tmp/b.vcd"),
                Option::<PathBuf>::None,
            ),
            Err(CaptureOutputPlanError::InvalidDslExtension { .. })
        ));
    }

    #[test]
    fn build_dsl_export_request_populates_session_and_simple_trigger() {
        let snapshot = sample_snapshot();
        let effective = CaptureDeviceOptionSnapshot {
            operation_mode_id: "buffer".to_string(),
            stop_option_id: Some("immediate".to_string()),
            channel_mode_id: "16ch_100mhz".to_string(),
            enabled_channels: vec![0, 2],
            threshold_volts: Some(1.5),
            filter_id: Some("none".to_string()),
            sample_rate_hz: 100_000_000,
            sample_limit: 2048,
        };
        let mut channels = TriggerChannelConditions::new();
        channels.insert(2, TriggerCondition::Rising);
        let mut simple = [b'X'; TRIGGER_PROBE_CAPACITY];
        simple[2] = b'R';
        let outcome = CaptureTriggerOutcome {
            requested: ValidatedTrigger {
                spec: TriggerSpec::Simple { channels },
                position_percent: 25,
                program: dsview_sys::TriggerProgram {
                    mode: TriggerProgramMode::Simple,
                    position_percent: 25,
                    stage_selector: 0,
                    simple,
                    stages: Vec::new(),
                },
            },
            observation: TriggerObservation {
                saw_trigger_packet: true,
                packet_status: Some(AcquisitionPacketStatus::Ok),
                position: Some(TriggerPosition {
                    raw_status: 1,
                    real_pos: 512,
                    remain_count: 0,
                }),
            },
        };

        let request = build_dsl_export_request(&DslSessionMetadataInput {
            title: "dsview-cli test",
            device_model: "DSLogic Plus",
            samplerate_hz: 100_000_000,
            sample_count: 2048,
            enabled_channels: &[0, 2],
            capture_started_at: UNIX_EPOCH,
            device_options_snapshot: &snapshot,
            effective_options: &effective,
            trigger: Some(&outcome),
        });

        assert_eq!(request.trigger_pos, 512);
        let session: Value = serde_json::from_str(&request.session_json).unwrap();
        assert_eq!(session["Version"], 3);
        assert_eq!(session["Operation Mode"], "Buffer Mode");
        assert_eq!(session["Channel Mode"], "Use 16 Channels (Max 100MHz)");
        assert_eq!(session["Threshold Level"], "1.5");
        assert_eq!(session["channel"][0]["index"], 0);
        assert_eq!(session["channel"][0]["strigger"], 0);
        assert_eq!(session["channel"][1]["index"], 2);
        assert_eq!(session["channel"][1]["strigger"], 1);
        assert_eq!(session["trigger"]["triggerPos"], 25);
    }
}
