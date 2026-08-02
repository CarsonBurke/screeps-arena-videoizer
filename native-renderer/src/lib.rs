//! Native, fail-closed frontend for the Screeps Arena temporal renderer.
//!
//! It validates the exact ReplayIR/renderer-contract pair, turns the timeline
//! into independently addressable frame samples, prepares a cached texture
//! atlas, and exposes a bounded multiview sprite submission path. Full renderer
//! semantic lowering and hardware video encoding are still under development.

mod action_manager;
mod action_plan;
mod action_runtime;
mod artifact;
mod assets;
mod atlas_cache;
mod creep_actions;
mod error;
mod gpu;
mod layer_compositor;
mod mip;
mod nv12;
mod procedural_graphics;
mod rational;
mod renderer_random;
mod scene_geometry;
mod scene_nodes;
mod scene_plan;
mod scene_resolve;
mod scene_runtime;
mod scene_schedule;
mod scene_values;
mod temporal_batch;
mod terrain;
mod terrain_blur;
mod terrain_draw;
mod terrain_gpu;
mod terrain_paint;
mod terrain_raster;
mod terrain_wall;
mod text_raster;
mod timeline;
mod value_plan;
mod vector_gpu;
mod vector_graphics;
mod vector_tessellation;
mod video;
#[cfg(target_os = "linux")]
mod vulkan_external_nv12;
#[cfg(target_os = "linux")]
mod vulkan_nvenc;

pub use action_manager::ActionManagerRuntime;
pub use action_plan::{
    ActionGroupPlan, ActionKind, ActionNode, ActionParameter, RETAINED_ACTION_TYPES,
    ResolvedActionNode, ResolvedActionParameter,
};
pub use action_runtime::{ActionEasing, ActionRuntime, ActionTarget};
pub use artifact::{
    BoardFrame, Entity, IndexedReplay, NonFiniteEntry, Nullable, RenderConfig, RendererContract,
    RendererEvent, RendererEventIter, RendererEventOpcode, ReplayArtifact, ReplayIr, Track,
    TrackValue,
};
pub use assets::{
    AtlasEntry, AtlasOptions, AtlasRasterAsset, TextureAtlas, TextureAtlasPage,
    decoration_asset_name,
};
pub use atlas_cache::atlas_cache_filename;
pub use error::{Error, Result};
pub use gpu::{
    EncodedTemporalBatch, FrameConfig, GpuTextureAtlas, LeasedTerrainPhase, PIXI_COLOR_FORMAT,
    PendingTemporalReadback, SPRITE_BLUR_SHADER, SPRITE_SHADER, SpriteBlendMode, SpriteDrawRun,
    SpriteInstance, SpritePipeline, TemporalBatchLease, TemporalRenderBatch,
    TemporalSpriteRenderer, TemporalSubmission, TemporalTarget, TemporalTerrainCache,
    validate_sprite_shader,
};
pub use layer_compositor::{
    LAYER_COMPOSITE_SHADER, TemporalLayerCompositor, TemporalLightingSource,
    validate_layer_composite_shader,
};
pub use nv12::{
    NV12_SHADER, Nv12BatchConverter, Nv12ReadbackBuffer, Nv12ReadbackLayout, PACKED_NV12_SHADER,
    PackedNv12Converter, rgba8_to_nv12_reference, validate_nv12_shader,
};
pub use procedural_graphics::procedural_graphics_assets;
pub use rational::Rational;
pub use renderer_random::RendererRandom;
pub use scene_geometry::{Affine2, BoardTransform};
pub use scene_nodes::{
    NodeTransform, PreparedSprite, PreparedSpriteInstance, PreparedVector, SceneDisplayEntry,
    SceneDrawableKind, SceneFrameScratch, SceneNodeKey, SceneNodeKind, SceneNodeTemplate,
    SceneNodeTemplates, SpriteDisplayEntry,
};
pub use scene_plan::{
    ObjectPlan, ProcessorKind, ProcessorPlan, RETAINED_PROCESSOR_TYPES, RendererLayerPlan,
    RendererPlan, SlotBudget,
};
pub use scene_resolve::{ResolvedActivation, ResolvedScene};
pub use scene_runtime::{GenericSceneRuntime, TemporalSceneBatch, TemporalSceneStats};
pub use scene_schedule::{
    ActionInterval, ObjectInterval, ProcessorInterval, SceneActivation, SceneSchedule,
};
pub use scene_values::EntityValueRoots;
pub use temporal_batch::TemporalSpriteBatch;
pub use terrain::{
    TerrainGeometry, TerrainGeometryCompiler, TerrainGeometrySpan, TerrainGeometryTimeline,
    TerrainSwampTexture,
};
pub use terrain_blur::{
    GpuTerrainBlurBank, TERRAIN_BLUR_SHADER, TerrainBlurBindings, TerrainBlurRequest,
    validate_terrain_blur_shader,
};
pub use terrain_draw::{
    TerrainCoverage, TerrainDrawOp, TerrainDrawPhase, TerrainDrawPlan, TerrainDrawSource,
    TerrainLayerComposite, TerrainPlacement, TerrainTextureSample,
};
pub use terrain_gpu::{
    DEFAULT_TERRAIN_BANK_BYTE_BUDGET, GpuTerrainMaskBank, TERRAIN_DRAW_SHADER, TERRAIN_MASK_FORMAT,
    TemporalTerrainBatch, TemporalTerrainSceneBatch, TemporalTerrainSceneInput,
    TerrainCommandUploads, TerrainEncodePass, TerrainGpuBindings, TerrainGpuInstance,
    TerrainMaskBindings, TerrainPipeline, validate_terrain_draw_shader,
};
pub use terrain_paint::{
    TerrainFramePaint, TerrainLightingMode, TerrainNoisePaint, TerrainPaintStyle,
    TerrainRampartPaint, TerrainTexturePaint,
};
pub use terrain_raster::{
    TerrainRasterCache, TerrainRasterCacheStats, TerrainRasterMask, TerrainRasterMasks,
    TerrainRasterStyle,
};
pub use terrain_wall::{GpuTerrainWallBank, TerrainWallRequest};
pub use timeline::{
    AdvanceStep, FrameBatch, FrameBatchIter, FrameSample, Timeline, TimelineEvent,
    TimelineEventIter,
};
pub use value_plan::{
    CompiledValue, ExpressionOperator, ExpressionPlan, RETAINED_EXPRESSION_OPERATORS,
    ResolvedValue, ValueContext,
};
pub use vector_gpu::{
    MAX_TEMPORAL_VECTOR_INSTANCES, MAX_TEMPORAL_VECTOR_VERTICES, MAX_VECTOR_GPU_BYTES,
    TEMPORAL_VECTOR_SHADER, TemporalVectorBatch, VECTOR_FILTER_SHADER, VectorDrawRun,
    VectorGpuInstance, VectorGpuVertex, VectorPipeline, validate_vector_shader,
};
pub use vector_graphics::{
    RETAINED_DRAW_METHODS, VectorCommand, VectorFillStyle, VectorLineStyle, VectorProgram,
    site_progress_program, vector_graphics_programs,
};
pub use vector_tessellation::{
    VectorGeometryId, VectorMesh, VectorVertex, tessellate_vector_program,
};
pub use video::{
    FfmpegAv1Muxer, FfmpegVideoEncoder, VideoCodec, VideoEncoderConfig, VideoEncoderStats,
};
#[cfg(target_os = "linux")]
pub use vulkan_external_nv12::{VulkanExternalNv12, VulkanExternalNv12Error};
#[cfg(target_os = "linux")]
pub use vulkan_nvenc::{
    EncodedAv1Frame, VulkanNvencConfig, VulkanNvencEncoder, VulkanNvencError, VulkanNvencRing,
};
