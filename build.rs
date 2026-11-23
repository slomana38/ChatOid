#[path = "src/branding.rs"]
mod branding;

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/branding.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    for (name, agent) in [("chatoid", false), ("chatoid-agent", true)] {
        let ico = directory.join(format!("{name}.ico"));
        let rc = directory.join(format!("{name}.rc"));
        fs::write(&ico, icon_file(agent)).expect("write generated ChatOid icon");
        let icon_path = ico.to_str().expect("resource path").replace('\\', "/");
        fs::write(
            &rc,
            format!("#pragma code_page(65001)\n1 ICON \"{icon_path}\"\n"),
        )
        .expect("write resource script");
        embed_resource::compile_for(&rc, [name], embed_resource::NONE)
            .manifest_required()
            .expect("embed the Windows executable icon");
    }
}

fn icon_file(agent: bool) -> Vec<u8> {
    let sizes = [16_u32, 24, 32, 48, 64, 128];
    let mut header = vec![0, 0, 1, 0, sizes.len() as u8, 0];
    let mut images = Vec::new();
    for size in sizes {
        let rgba = branding::icon_rgba(agent, size);
        let mask_stride = size.div_ceil(32) * 4;
        let source_offset = (6 + sizes.len() * 16 + images.len()) as u32;
        let image_size = 40 + (size * 4 + mask_stride) * size;
        header.extend([size as u8, size as u8, 0, 0, 1, 0, 32, 0]);
        header.extend(image_size.to_le_bytes());
        header.extend(source_offset.to_le_bytes());
        for value in [40, size, size * 2] {
            images.extend(value.to_le_bytes());
        }
        images.extend([1, 0, 32, 0]);
        for _ in 0..6 {
            images.extend(0_u32.to_le_bytes());
        }
        for row in rgba.chunks_exact((size * 4) as usize).rev() {
            for pixel in row.as_chunks::<4>().0 {
                images.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
        for row in rgba.chunks_exact((size * 4) as usize).rev() {
            let mut mask = vec![0; mask_stride as usize];
            for (column, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                if pixel[3] == 0 {
                    mask[column / 8] |= 0x80 >> (column % 8);
                }
            }
            images.extend(mask);
        }
    }
    header.extend(images);
    header
}
