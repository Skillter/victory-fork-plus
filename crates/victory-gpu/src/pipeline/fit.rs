//! The mesh outputs a pair declares, when D3D12's mesh output cap refuses
//! them.
//!
//! A mesh program that declares more components than the cap allows at the
//! group's maxima (65 at 128 vertices; four bytes a component whatever its
//! type) cannot be translated as it is, and no repacking helps. The pipeline is
//! the first place the program is known together with the pixel program drawn
//! after it, so it is here that the mesh's outputs are cut to what that pixel
//! program reads (`victory_shader::emit::fit`).
//!
//! The trimmed containers go back through [`super::shader`], which keys a
//! container by its bytes: the narrowed pair gets its own translation, its own
//! shader-cache entry and its own pipeline-library key, so a pipeline made from
//! a cut pair can never be served for a pair that was not cut.

use std::sync::Arc;

use super::Shader;

/// `shaders` with the mesh program's outputs cut to what the pixel program
/// reads: `None` when the pair needs no cut, or when no cut fits it (then the
/// mesh program is translated as it is and reports why).
pub(crate) fn fit(shaders: &[Arc<Shader>]) -> Option<Vec<Arc<Shader>>> {
    let mesh = shaders.iter().find(|s| s.stage == "mesh")?;
    let pixel = shaders.iter().find(|s| s.stage == "pixel")?;
    let containers = victory_shader::emit::fit::pair(&mesh.container, &pixel.container)?;
    Some(
        shaders
            .iter()
            .map(|s| match s.stage {
                // Both stages are cut, so they still declare the same
                // components and D3D12 links them.
                "mesh" => super::shader("mesh", &containers.0),
                "pixel" => super::shader("pixel", &containers.1),
                _ => s.clone(),
            })
            .collect(),
    )
}
