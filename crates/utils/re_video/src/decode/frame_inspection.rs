use h264_reader::{
    annexb::AnnexBReader,
    nal::{self, Nal as _},
    push::NalInterest,
};

use crate::{VideoCodec, VideoEncodingDetails, h264::encoding_details_from_h264_sps};

/// Failure reason for [`detect_gop_start`].
#[derive(thiserror::Error, Debug)]
pub enum VideoChunkInspectionError {
    #[error("Detection not supported for codec: {0:?}")]
    UnsupportedCodec(VideoCodec),

    #[error("NAL header error: {0:?}")]
    NalHeaderError(h264_reader::nal::NalHeaderError),

    #[error("Detected group of picture but failed to extract encoding details: {0:?}")]
    FailedToExtractEncodingDetails(String),
}

impl PartialEq<Self> for VideoChunkInspectionError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::UnsupportedCodec(a), Self::UnsupportedCodec(b)) => a == b,
            (Self::NalHeaderError(_), Self::NalHeaderError(_)) => true, // `NalHeaderError` isn't implementing PartialEq, but there's only one variant.
            (Self::FailedToExtractEncodingDetails(a), Self::FailedToExtractEncodingDetails(b)) => {
                a == b
            }
            _ => false,
        }
    }
}

impl Eq for VideoChunkInspectionError {}

/// Result of a successful GOP detection.
///
/// I.e. whether a sample is the start of a GOP and if so, encoding details we were able to extract from it.
#[derive(Default, PartialEq, Eq, Debug)]
pub enum GopStartDetection {
    /// The sample is the start of a GOP and encoding details have been extracted.
    StartOfGop(VideoEncodingDetails),

    /// The sample is not the start of a GOP.
    #[default]
    NotStartOfGop,
}

/// Result of a successful inspection of a video chunk.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct VideoChunkInspection {
    /// Whether this chunk is the start of a GOP.
    pub gop_detection: GopStartDetection,

    /// If we're able to detect it, the number of frames in this chunk.
    ///
    /// More than one frame is currently an input data bug since
    /// we expect exactly one frame per chunk.
    // TODO(andreas): We could go one step further and extract the frame byte offsets to split those chunks up?
    pub num_frames_detected: Option<usize>,
}

/// Try to determine whether a frame chunk is the start of a GOP.
///
/// This is a best effort attempt to determine this, but we won't always be able to.
#[inline]
pub fn inspect_video_chunk(
    sample_data: &[u8],
    codec: VideoCodec,
) -> Result<VideoChunkInspection, VideoChunkInspectionError> {
    #[expect(clippy::match_same_arms)]
    match codec {
        VideoCodec::H264 => inspect_h264_annexb_sample(sample_data),
        VideoCodec::H265 => Err(VideoChunkInspectionError::UnsupportedCodec(codec)),
        VideoCodec::AV1 => Err(VideoChunkInspectionError::UnsupportedCodec(codec)),
        VideoCodec::VP8 => Err(VideoChunkInspectionError::UnsupportedCodec(codec)),
        VideoCodec::VP9 => Err(VideoChunkInspectionError::UnsupportedCodec(codec)),
    }
}

#[derive(Default)]
struct H264InspectionState {
    coding_details_from_sps: Option<Result<VideoEncodingDetails, String>>,
    idr_frame_found: bool,
    num_frames_detected: usize,
}

impl h264_reader::push::AccumulatedNalHandler for H264InspectionState {
    fn nal(&mut self, nal: nal::RefNal<'_>) -> NalInterest {
        let Ok(nal_header) = nal.header() else {
            return NalInterest::Ignore;
        };
        let nal_unit_type = nal_header.nal_unit_type();

        match nal_unit_type {
            nal::UnitType::SeqParameterSet => {
                if !nal.is_complete() {
                    // Want full SPS, not just a partial one in order to extract the encoding details.
                    return NalInterest::Buffer;
                }

                // Note that if we find several SPS, we'll always use the latest one.
                self.coding_details_from_sps = Some(
                    match nal::sps::SeqParameterSet::from_bits(nal.rbsp_bits())
                        .and_then(|sps| encoding_details_from_h264_sps(&sps))
                    {
                        Ok(coding_details) => {
                            // A bit too much string concatenation something that frequent, better to enable this only for debug builds.
                            if cfg!(debug_assertions) {
                                re_log::trace!(
                                    "Parsed SPS to coding details for video stream: {coding_details:?}"
                                );
                            }
                            Ok(coding_details)
                        }
                        Err(sps_error) => Err(format!("Failed reading SPS: {sps_error:?}")), // h264 errors don't implement display
                    },
                );

                NalInterest::Ignore
            }

            nal::UnitType::SliceLayerWithoutPartitioningIdr => {
                self.idr_frame_found = true;
                self.num_frames_detected += 1;
                NalInterest::Ignore
            }

            nal::UnitType::SliceLayerWithoutPartitioningNonIdr => {
                self.num_frames_detected += 1;
                NalInterest::Ignore
            }

            _ => NalInterest::Ignore,
        }
    }
}

/// Try to determine whether a frame chunk is the start of a closed GOP in an h264 Annex B encoded stream.
fn inspect_h264_annexb_sample(
    mut sample_data: &[u8],
) -> Result<VideoChunkInspection, VideoChunkInspectionError> {
    let mut reader = AnnexBReader::accumulate(H264InspectionState::default());

    while !sample_data.is_empty() {
        // Don't parse everything at once.
        const MAX_CHUNK_SIZE: usize = 256;
        let chunk_size = MAX_CHUNK_SIZE.min(sample_data.len());

        reader.push(&sample_data[..chunk_size]);
        sample_data = &sample_data[chunk_size..];
    }

    let handler = reader.into_nal_handler();

    let gop_detection = match handler.coding_details_from_sps {
        Some(Ok(coding_details)) => {
            if handler.idr_frame_found {
                GopStartDetection::StartOfGop(coding_details)
            } else {
                // In theory it could happen that we got an SPS but no IDR frame.
                // Arguably we should preserve the information from the SPS, but practically it's not useful:
                // If we never hit an IDR frame, then we can't play the video and every IDR frame is supposed to have
                // the *same* SPS.
                GopStartDetection::NotStartOfGop
            }
        }
        Some(Err(error_str)) => {
            return Err(VideoChunkInspectionError::FailedToExtractEncodingDetails(
                error_str.clone(),
            ));
        }
        None => GopStartDetection::NotStartOfGop,
    };

    Ok(VideoChunkInspection {
        gop_detection,
        num_frames_detected: Some(handler.num_frames_detected),
    })
}

#[cfg(test)]
mod test {
    use super::{GopStartDetection, VideoChunkInspection, inspect_h264_annexb_sample};
    use crate::{ChromaSubsamplingModes, VideoChunkInspectionError, VideoEncodingDetails};

    #[test]
    fn test_detect_h264_annexb_gop() {
        // Example H.264 Annex B encoded data containing SPS and IDR frame. (ai generated)
        let sample_data = &[
            // SPS NAL unit
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0A, 0xAC, 0x72, 0x84, 0x44, 0x26, 0x84,
            0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xCA, 0x3C, 0x48, 0x96, 0x11,
            0x80, // IDR frame NAL unit
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21, 0x43, 0x02, 0x4C, 0x82, 0x54, 0x2B,
            0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92,
        ];
        let result = inspect_h264_annexb_sample(sample_data);
        assert_eq!(
            result,
            Ok(VideoChunkInspection {
                gop_detection: GopStartDetection::StartOfGop(VideoEncodingDetails {
                    codec_string: "avc1.64000A".to_owned(),
                    coded_dimensions: [64, 64],
                    bit_depth: Some(8),
                    chroma_subsampling: Some(ChromaSubsamplingModes::Yuv420),
                    stsd: None,
                }),
                num_frames_detected: Some(1),
            })
        );

        // Example H.264 Annex B encoded data containing broken SPS and IDR frame. (above example but messed with the SPS)
        let sample_data = &[
            0x00, 0x00, 0x00, 0x01, 0x67, // SPS NAL unit
            0x00, 0x00, 0x0A, 0xAC, 0x72, 0x84, 0x44, 0x26, 0x84, 0x00, 0x00, 0x03, 0x00, 0x04,
            0x00, 0x00, 0x03, 0x00, 0xCA, 0x3C, 0x48, 0x96, 0x11, 0x80, //
            0x00, 0x00, 0x00, 0x01, 0x65, // IDR frame NAL unit
            0x88, 0x84, 0x21, 0x43, 0x02, 0x4C, 0x82, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A,
            0x92, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92,
        ];
        let result = inspect_h264_annexb_sample(sample_data);
        assert_eq!(
            result,
            Err(VideoChunkInspectionError::FailedToExtractEncodingDetails(
                "Failed reading SPS: RbspReaderError(RemainingData)".to_owned()
            ))
        );

        // Example H.264 Annex B encoded data containing SPS, IDR and non-IDR frames.
        let sample_data = &[
            0x00, 0x00, 0x00, 0x01, 0x67, // SPS NAL unit
            0x64, 0x00, 0x0A, 0xAC, 0x72, 0x84, 0x44, 0x26, 0x84, 0x00, 0x00, 0x03, 0x00, 0x04,
            0x00, 0x00, 0x03, 0x00, 0xCA, 0x3C, 0x48, 0x96, 0x11, 0x80, //
            0x00, 0x00, 0x00, 0x01, 0x65, // IDR frame NAL unit
            0x88, 0x84, 0x21, 0x43, 0x02, 0x4C, 0x82, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A,
            0x92, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92, 0x80, //
            0x00, 0x00, 0x00, 0x01, 0x61, // Non-IDR frame NAL unit
            0x88, 0x84, 0x21, 0x43, 0x02, 0x4C, 0x82, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A,
            0x92, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92,
        ];
        let result = inspect_h264_annexb_sample(sample_data);
        assert_eq!(
            result,
            Ok(VideoChunkInspection {
                gop_detection: GopStartDetection::StartOfGop(VideoEncodingDetails {
                    codec_string: "avc1.64000A".to_owned(),
                    coded_dimensions: [64, 64],
                    bit_depth: Some(8),
                    chroma_subsampling: Some(ChromaSubsamplingModes::Yuv420),
                    stsd: None,
                }),
                num_frames_detected: Some(2),
            })
        );

        // Example H.264 Annex B encoded data containing two non-IDR frames.
        let sample_data = &[
            0x00, 0x00, 0x00, 0x01, 0x61, // Non-IDR frame NAL unit
            0x88, 0x84, 0x21, 0x43, 0x02, 0x4C, 0x82, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A,
            0x92, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92, //
            0x00, 0x00, 0x00, 0x01, 0x61, // Non-IDR frame NAL unit
            0x88, 0x84, 0x21, 0x43, 0x02, 0x4C, 0x82, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A,
            0x92, 0x54, 0x2B, 0x8F, 0x2C, 0x8C, 0x54, 0x4A, 0x92,
        ];
        let result = inspect_h264_annexb_sample(sample_data);
        assert_eq!(
            result,
            Ok(VideoChunkInspection {
                gop_detection: GopStartDetection::NotStartOfGop,
                num_frames_detected: Some(2),
            })
        );

        // Garbage data, still annex b shaped. (ai generated)
        let sample_data = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0A, 0xAC, 0x72, 0x84, 0x44, 0x26, 0x84,
            0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xCA, 0x3C, 0x48, 0x96, 0x11,
            0x80,
        ];
        let result = inspect_h264_annexb_sample(sample_data);
        assert_eq!(
            result,
            Ok(VideoChunkInspection {
                gop_detection: GopStartDetection::NotStartOfGop,
                num_frames_detected: Some(0),
            })
        );

        // Garbage data, no detectable nalu units.
        let sample_data = &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A];
        let result = inspect_h264_annexb_sample(sample_data);
        assert_eq!(
            result,
            Ok(VideoChunkInspection {
                gop_detection: GopStartDetection::NotStartOfGop,
                num_frames_detected: Some(0),
            })
        );
    }
}
