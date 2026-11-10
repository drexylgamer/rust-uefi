#![no_main]
#![no_std]

use tinyvec::ArrayVec;
use core::mem::MaybeUninit;
use core::fmt::Write;
use uefi::prelude::*;
use uefi::println;
use uefi::print;
use uefi::proto::console::text::Key;
use uefi::proto::console::text::ScanCode;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::media::file::{File, FileMode, FileAttribute, Directory};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::device_path::build::DevicePathBuilder;
use uefi::proto::device_path::DevicePath;
use uefi::proto::BootPolicy;
use uefi::Identify;
use uefi::Char16;

#[derive(Debug, Clone, Copy)]
pub struct Efi {
    pub id: u16,
    pub name: [Char16; 64],
    pub file_path: [Char16; 256],
    pub part: Handle,
}

impl Efi {
    pub fn new(id: u16, name_slice: &[Char16], path_slice: &[Char16], part: Handle) -> Self {
	let mut name = [Char16::default(); 64];
	let mut file_path = [Char16::default(); 256];
	let n_len = core::cmp::min(name_slice.len(), 64);
	name[..n_len].copy_from_slice(&name_slice[..n_len]);
	let p_len = core::cmp::min(path_slice.len(), 256);
	file_path[..p_len].copy_from_slice(&path_slice[..p_len]);
	Self {
	    id,
	    name,
	    file_path,
	    part,
	}
    }
}

fn u16_to_and_str(mut num: u16, buf: &mut [u8; 5]) -> &str {
    if num == 0 {
	return "0";
    }
    let mut i = 5;
    while num > 0 {
	i -= 1;
	buf[i] = b'0' + (num % 10) as u8;
	num /= 10
    }
    core::str::from_utf8(&buf[i..]).unwrap()
}

fn find_all_efi_parts() -> Result<ArrayVec<[Option<Efi>; 64]>, uefi::Error> {
    let mut efi_list: ArrayVec<[Option<Efi>; 64]> = ArrayVec::new();
    let handles = boot::locate_handle_buffer(boot::SearchType::ByProtocol(&SimpleFileSystem::GUID))?;
    let mut dynamic_id = 0;

    for handle in handles.iter() {
        if let Ok(mut fs) = boot::open_protocol_exclusive::<SimpleFileSystem>(*handle) {
            if let Ok(mut root) = fs.open_volume() {
                if root.open(cstr16!(r"\EFI"), FileMode::Read, FileAttribute::empty()).is_ok() {
                    let root_path = [Char16::default(); 256];
                    // Dynamic scanning feeds directly to our list
                    scan_for_efi_files(&mut root, &root_path, &mut efi_list, *handle, &mut dynamic_id);
                }
            }
        }
    }
    Ok(efi_list)
}

fn wait_for_keypress() -> u16 {
    uefi::system::with_stdin(|stdin| {
	let _ = stdin.reset(false);
	loop {
	    if let Ok(key_event) = stdin.wait_for_key_event() {
		let _ = boot::wait_for_event(&mut [key_event]);
	    }
	    if let Ok(Some(key)) = stdin.read_key() {
		match key {
		    Key::Printable(ch) => return u16::from(ch),
		    Key::Special(ScanCode::UP) => return 0x01,
		    Key::Special(ScanCode::DOWN) => return 0x02,
		    _ => {}
		}
	    }
	}
    })
}

fn clear_screen() {
    uefi::system::with_stdout(|stdout| {
	stdout.clear().unwrap();
    });
}

fn write_boilerplate_message() {
    println("\n\n   > ROOKIE'S BOOT MANAGER\n");
}

fn println(message: &str) {
    uefi::system::with_stdout(|stdout| {
	let _ = write!(stdout, "{message}\r\n");
    });
}

fn print(message: &str) {
    uefi::system::with_stdout(|stdout| {
	let _ = write!(stdout, "{message}");
    });
}

fn write_efis_from_value(value: u16, efis: &[Option<Efi>]) {
    for efi_opt in efis.iter() {
	if let Some(efi) = efi_opt {	  
	    print!("   [{}] ", efi.id);
	    for &ch in efi.file_path.iter() {
                if ch == Char16::default() { break; }
                uefi::print!("{}", char::from(ch));
            }
            if efi.id == value {
                uefi::print!(" <");
            }
            uefi::print!("\n");
	}
    }
}

fn wait_for_loop() {
    let efis: ArrayVec<[Option<Efi>; 64]> = find_all_efi_parts().expect("REASON2");

    let mut current_id = 0u16;
    loop {
	clear_screen();
	let _ = find_all_efi_parts().expect("REASON2");
	write_boilerplate_message();
	write_efis_from_value(current_id, &efis);
	let text = wait_for_keypress();
	if text == 0x01 {
	    if current_id > 0u16 {
		current_id-=1u16;
	    }
	}
	if text == 0x02 {
	    if current_id < efis.len() as u16 - 1 {
		current_id+=1u16;
	    }
	}
	if text == b'\r' as u16 {
	if let Some(efi) = &efis[current_id as usize] {
		uefi_logic(efi);
		break;
	}
	}
    }
}

fn set_max_text_mode() {
    uefi::system::with_stdout(|stdout| {
        let mut target_mode = None;
        let mut max_rows = 0;
        let mut max_cols = 0;

        // 1. Iterate over all modes using the native text mode iterator
        for mode in stdout.modes() {
            let cols = mode.columns();
            let rows = mode.rows();

            // Find the mode layout with the absolute highest row count (smallest font scale grid)
            if rows > max_rows || (rows == max_rows && cols > max_cols) {
                max_rows = rows;
                max_cols = cols;
                target_mode = Some(mode);
            }
        }

        // 2. Apply the chosen mode configuration if one was located
        if let Some(mode) = target_mode {
            let _ = stdout.set_mode(mode);
        }
    });
}

fn scan_for_efi_files(
    dir: &mut Directory, 
    current_path: &[Char16], 
    efi_list: &mut ArrayVec<[Option<Efi>; 64]>,
    part_handle: Handle,
    id_counter: &mut u16,
) {
    let mut buffer = [0u8; 512];

    loop {
        match dir.read_entry(&mut buffer) {
            Ok(Some(info)) => {
                let name = info.file_name();
                if name == cstr16!(".") || name == cstr16!("..") {
                    continue;
                }

                // 1. Handle nested subdirectories recursively
                if info.attribute().contains(FileAttribute::DIRECTORY) {
                    if let Ok(file_handle) = dir.open(name, FileMode::Read, FileAttribute::DIRECTORY) {
                        if let Some(mut nested_dir) = file_handle.into_directory() {
                            let mut next_path = [Char16::default(); 256];
                            let mut cursor = 0;
                            
                            for &ch in current_path {
                                if ch == Char16::default() || cursor >= 250 { break; }
                                next_path[cursor] = ch;
                                cursor += 1;
                            }
                            for &ch in name.iter() {
                                if ch == Char16::default() || cursor >= 250 { break; }
                                next_path[cursor] = ch;
                                cursor += 1;
                            }
                            if let Some(backslash) = Char16::try_from('\\').ok() {
                                next_path[cursor] = backslash;
                                cursor += 1;
                            }

                            scan_for_efi_files(&mut nested_dir, &next_path, efi_list, part_handle, id_counter);
                        }
                    }
                } else {
                    // 2. Handle files
                    let slice = name.as_slice();
                    let len = slice.len();
                    
                    let mut is_bootable = false;

                    // Match standard .efi files
                    if len > 4 {
                        is_bootable = (slice[len - 4] == '.') &&
                                      (slice[len - 3] == 'e' || slice[len - 3] == 'E') &&
                                      (slice[len - 2] == 'f' || slice[len - 2] == 'F') &&
                                      (slice[len - 1] == 'i' || slice[len - 1] == 'I');
                    }

                    // 3. Register the file if it passed any boot constraints
                    if is_bootable && efi_list.len() < 64 {
                        let mut full_path = [Char16::default(); 256];
                        let mut idx = 0;

                        // Append current folder path tree strings
                        for &ch in current_path {
                            if ch == Char16::default() || idx >= 250 { break; }
                            full_path[idx] = ch;
                            idx += 1;
                        }

                        // Append file name characters strings
                        for &ch in slice {
                            if ch == Char16::default() || idx >= 250 { break; }
                            full_path[idx] = ch;
                            idx += 1;
                        }

                        let entry = Efi::new(*id_counter, slice, &full_path, part_handle);
                        efi_list.push(Some(entry));
                        *id_counter += 1;
                    }
                }
            }
            Ok(None) => break, 
            Err(_) => break,
        }
    }
}

fn uefi_logic(efi: &Efi) {
    clear_screen();
    uefi::print!("Booting custom target path: ");
    
    let mut exact_len = 0;
    for &ch in efi.file_path.iter() {
        if ch == Char16::default() { break; }
        exact_len += 1;
        uefi::print!("{}", char::from(ch));
    }
    uefi::println!("\n");

    let partition_device_path = boot::open_protocol_exclusive::<DevicePath>(efi.part)
	.expect("Failed to get part hardware device path");

    let mut path_buffer = [MaybeUninit::<u8>::uninit(); 512];
    let mut builder = DevicePathBuilder::with_buf(&mut path_buffer);

    for node in partition_device_path.node_iter() {
	builder = builder.push(&node).unwrap();
    }

    let raw_u16_slice: &[u16] = unsafe {
        core::slice::from_raw_parts(efi.file_path.as_ptr() as *const u16, exact_len + 1)
    };

    let full_cstr16 = uefi::CStr16::from_u16_with_nul(raw_u16_slice)
        .expect("Failed to parse dynamic string slice bounds cleanly");

    let clean_boot_path = if full_cstr16.as_slice().first().map_or(false, |&c| c == '\\') {
        let slice_without_leading_slash = &full_cstr16.as_slice_with_nul();
        let raw_sub_u16: &[u16] = unsafe {
            core::slice::from_raw_parts(slice_without_leading_slash.as_ptr() as *const u16, slice_without_leading_slash.len())
        };
        uefi::CStr16::from_u16_with_nul(raw_sub_u16).unwrap()
    } else {
        full_cstr16
    };

    let file_node = uefi::proto::device_path::build::media::FilePath {
	path_name: clean_boot_path,
    };

    let complete_device_path = builder
	.push(&file_node)
	.unwrap()
	.finalize()
	.unwrap();

    let loaded_image_handle = boot::load_image(
	boot::image_handle(),
	boot::LoadImageSource::FromDevicePath {
	    device_path: complete_device_path,
	    boot_policy: BootPolicy::ExactMatch,
	},
    ).expect("Failed to load target img into memory");
    boot::start_image(loaded_image_handle).expect("Failed to execute image");
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    set_max_text_mode();
    clear_screen();
    write_boilerplate_message();
    wait_for_loop();
    Status::SUCCESS
}
