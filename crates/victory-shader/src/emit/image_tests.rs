//! Image operations lowered to DXIL, checked against `dxil.dll` where it is
//! installed.

use super::container::SigElement;
use super::lower::{Lowered, Parts, lower};
use super::validator::Validator;
use crate::native::{
    HwStage, NativeProgram, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// Root parameter `slot` in user SGPRs from `user_data` on.
fn root(root: RootType, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: if root == RootType::Descriptor { 2 } else { 1 },
        base_register: 0,
        user_data,
        slot,
    }
}

/// Lowers a pixel program with the root parameters `layout` (in user SGPRs
/// from s0 on), writing one `SV_Target` of four floats.
///
/// The output is not decoration: `dxil.dll` accepted a pixel module that
/// writes nothing where it refused the same module with an output, so a pixel
/// test that exports nothing proves nothing.
fn lower_ps(
    words: &[u32],
    layout: Vec<RootParam>,
    used: Vec<UsedResource>,
) -> Result<Lowered, crate::ShaderError> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 256,
        layout,
        used,
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code,
    };
    let p = ir::build(&native.code, false, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 0,
            half: false,
        }],
        prims: vec![],
        mesh_stride: 0,
    })
}

/// A level query (`image_get_lod`) of a 2D array texture. The console's
/// coordinate is (u, v, slice); DXIL's `calculateLOD` takes no slice — the
/// layer does not select a level — and wants an `undef` where it would be.
/// Handing it the layer was dxil.dll's "out of bound coord must be undef",
/// the last of the title's translation failures: three programs, every one a
/// level query of a `Texture2DArray` (`PS_VfxVBuffer_AlphaClip_0e67afa3_Wrapped`).
#[test]
fn a_level_query_passes_no_array_slice() {
    let words = [
        0xBE82_0309, // s_mov_b32 s2, s9
        0xBE89_0380, // s_mov_b32 s9, 0
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF408_0204,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[8:9], null (the sampler)
        0xF40C_0001,
        0xFA00_0000, // s_load_dwordx8 s[0:7], s[2:3], null (the texture)
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7E06_0280, // v_mov_b32 v3, 0
        0x7E08_0281, // v_mov_b32 v4, 1 (the array slice)
        0xF180_0128,
        0x0040_0002, // image_get_lod v0, v[2:4], s[0:7], s[8:11]
        0xF800_180F,
        0x0000_0000, // exp mrt0, v0, v0, v0, v0 done vm
        S_ENDPGM,
    ];
    let layout = vec![root(RootType::Table, 8, 0), root(RootType::Table, 9, 1)];
    let l = lower_ps(&words, layout, vec![]).expect("a level query lowers");
    assert!(
        l.declared.iter().any(|n| n == "dx.op.calculateLOD.f32"),
        "the program is a level query"
    );
    // The oracle: dxil.dll refused this module with the message above, the
    // layer of the array as coordinate 2.
    if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
        v.validate(&mut l.dxil.clone()).expect("a valid level query");
    }
}

