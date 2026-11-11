//! The stack a group will become (spec §5.5, §5.6), predicted from its
//! members' headers: what the inspector shows for a group before it is
//! written.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{MIN_LAYERS, MemberDecoding, file_stem, is_file_name, output_name};
use crate::decode::Probe;
use crate::exif::ExifData;
use crate::group::{CanvasProfile, Container, profile};
use crate::model::{FileKind, OutputFormat, RawTone};

/// One member of a group whose stack is predicted.
#[derive(Debug, Clone, Copy)]
pub struct PredictMember<'a> {
    /// The file id the prediction gives it by.
    pub id: usize,
    pub path: &'a Path,
    /// The kind it is decoded as, like [`FileRecord::kind`](crate::FileRecord::kind).
    pub kind: FileKind,
    /// Gives its orientation and, for a JPEG, its colour hint.
    pub exif: Option<&'a ExifData>,
}

/// What a member's headers say its layer will be.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberProbe {
    /// Upright, after its EXIF orientation.
    pub width: u32,
    pub height: u32,
    pub sixteen_bit: bool,
    pub has_alpha: bool,
}

/// The stack a group will become, as far as its members' headers tell.
#[derive(Debug, Clone, PartialEq)]
pub struct StackPrediction {
    /// One per member, in the order given: its probe, or why it failed.
    pub members: Vec<(usize, Result<MemberProbe, String>)>,
    /// `None` when no member could be probed.
    pub canvas: Option<(u32, u32)>,
    pub sixteen_bit: bool,
    pub has_alpha: bool,
    pub container: Container,
    /// The ids at the canvas resolution, in layer order.
    pub kept: Vec<usize>,
    /// `<first kept stem>_<kept>.<ext>`; `None` with fewer than 2 kept.
    pub file_name: Option<String>,
    pub icc_mismatch: bool,
}

/// Predicts the stack of `members`, given in layer order (`[0]` is the top
/// layer), in `format`, with RAWs in `tone`. `None` when `stop` was set
/// between two members.
///
/// Each member is probed exactly as the writer probes it (the same
/// orientation, tone and colour hint); one whose probe fails is in
/// [`StackPrediction::members`] with its error and takes no further part, as
/// the writer leaves it out with `LayerUnreadable`. The canvas profile
/// (spec §5.5) of the others gives the canvas, depth, alpha, container and
/// kept members, and the name is [`output_name`] at attempt 0 after the
/// first kept member's file stem; a stem that is not a file name gives no
/// name, as the writer refuses it.
///
/// The name is only a prediction: a name already taken gets a `-1` suffix,
/// and a member whose full decode then fails lowers the layer count.
///
/// It decodes no pixel and takes no memory permit, but it reads every
/// member's file, so it must not be called on a UI thread. `stop` is
/// checked before each member is probed.
pub fn predict_stack(
    members: &[PredictMember<'_>],
    format: OutputFormat,
    tone: RawTone,
    stop: &AtomicBool,
) -> Option<StackPrediction> {
    let mut predicted = Vec::with_capacity(members.len());
    // By position in `members`, as the writer profiles its inputs, so a
    // repeated id cannot pick the wrong member's stem.
    let mut probes: Vec<(usize, Probe)> = Vec::with_capacity(members.len());
    for (i, member) in members.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        match MemberDecoding::of(member.kind, member.exif, tone).probe(member.path) {
            Ok(probe) => {
                predicted.push((member.id, Ok(member_probe(&probe))));
                probes.push((i, probe));
            }
            Err(error) => predicted.push((member.id, Err(error.to_string()))),
        }
    }

    let canvas = profile(&probes, format);
    Some(StackPrediction {
        members: predicted,
        canvas: (!probes.is_empty()).then_some((canvas.width, canvas.height)),
        sixteen_bit: canvas.sixteen_bit,
        has_alpha: canvas.has_alpha,
        container: canvas.container,
        kept: canvas.kept.iter().map(|&i| members[i].id).collect(),
        file_name: predicted_name(members, &canvas),
        icc_mismatch: canvas.icc_mismatch,
    })
}

fn member_probe(probe: &Probe) -> MemberProbe {
    MemberProbe {
        width: probe.width,
        height: probe.height,
        sixteen_bit: probe.sixteen_bit,
        has_alpha: probe.has_alpha,
    }
}

/// The name the writer would first try for the kept members of `canvas`
/// (positions in `members`), or `None` when it would skip the stack or
/// refuse the stem.
fn predicted_name(members: &[PredictMember<'_>], canvas: &CanvasProfile) -> Option<String> {
    if canvas.kept.len() < MIN_LAYERS {
        return None;
    }
    let stem = file_stem(members[canvas.kept[0]].path);
    is_file_name(&stem).then(|| output_name(&stem, canvas.kept.len(), canvas.container, 0))
}
