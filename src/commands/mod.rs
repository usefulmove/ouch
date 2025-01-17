//! Receive command from the cli and call the respective function for that command.

mod compress;
mod decompress;
mod list;

use std::{
    io::Write,
    ops::ControlFlow,
    path::{Path, PathBuf},
};

use fs_err as fs;
use utils::colors;

use crate::{
    commands::{compress::compress_files, decompress::decompress_file, list::list_archive_contents},
    error::FinalError,
    extension::{self, flatten_compression_formats, Extension, SUPPORTED_EXTENSIONS},
    info,
    list::ListOptions,
    utils::{
        self, dir_is_empty, pretty_format_list_of_paths, to_utf, try_infer_extension, user_wants_to_continue,
        FileVisibilityPolicy,
    },
    warning, Opts, QuestionAction, QuestionPolicy, Subcommand,
};

/// Warn the user that (de)compressing this .zip archive might freeze their system.
fn warn_user_about_loading_zip_in_memory() {
    const ZIP_IN_MEMORY_LIMITATION_WARNING: &str = "\n\
        \tThe format '.zip' is limited and cannot be (de)compressed using encoding streams.\n\
        \tWhen using '.zip' with other formats, (de)compression must be done in-memory\n\
        \tCareful, you might run out of RAM if the archive is too large!";

    warning!("{}", ZIP_IN_MEMORY_LIMITATION_WARNING);
}

fn represents_several_files(files: &[PathBuf]) -> bool {
    let is_non_empty_dir = |path: &PathBuf| {
        let is_non_empty = || !dir_is_empty(path);

        path.is_dir().then(is_non_empty).unwrap_or_default()
    };

    files.iter().any(is_non_empty_dir) || files.len() > 1
}

/// Builds a suggested output file in scenarios where the user tried to compress
/// a folder into a non-archive compression format, for error message purposes
///
/// E.g.: `build_suggestion("file.bz.xz", ".tar")` results in `Some("file.tar.bz.xz")`
fn build_archive_file_suggestion(path: &Path, suggested_extension: &str) -> Option<String> {
    let path = path.to_string_lossy();
    let mut rest = &*path;
    let mut position_to_insert = 0;

    // Walk through the path to find the first supported compression extension
    while let Some(pos) = rest.find('.') {
        // Use just the text located after the dot we found
        rest = &rest[pos + 1..];
        position_to_insert += pos + 1;

        // If the string contains more chained extensions, clip to the immediate one
        let maybe_extension = {
            let idx = rest.find('.').unwrap_or(rest.len());
            &rest[..idx]
        };

        // If the extension we got is a supported extension, generate the suggestion
        // at the position we found
        if SUPPORTED_EXTENSIONS.contains(&maybe_extension) {
            let mut path = path.to_string();
            path.insert_str(position_to_insert - 1, suggested_extension);

            return Some(path);
        }
    }

    None
}

/// This function checks what command needs to be run and performs A LOT of ahead-of-time checks
/// to assume everything is OK.
///
/// There are a lot of custom errors to give enough error description and explanation.
pub fn run(
    args: Opts,
    question_policy: QuestionPolicy,
    file_visibility_policy: FileVisibilityPolicy,
) -> crate::Result<()> {
    match args.cmd {
        Subcommand::Compress {
            mut files,
            output: output_path,
        } => {
            // If the output_path file exists and is the same as some of the input files, warn the user and skip those inputs (in order to avoid compression recursion)
            if output_path.exists() {
                deduplicate_input_files(&mut files, &fs::canonicalize(&output_path)?);
            }
            // After cleaning, if there are no input files left, exit
            if files.is_empty() {
                return Err(FinalError::with_title("No files to compress").into());
            }

            // Formats from path extension, like "file.tar.gz.xz" -> vec![Tar, Gzip, Lzma]
            let mut formats = extension::extensions_from_path(&output_path);

            if formats.is_empty() {
                let error = FinalError::with_title(format!("Cannot compress to '{}'.", to_utf(&output_path)))
                    .detail("You shall supply the compression format")
                    .hint("Try adding supported extensions (see --help):")
                    .hint(format!("  ouch compress <FILES>... {}.tar.gz", to_utf(&output_path)))
                    .hint(format!("  ouch compress <FILES>... {}.zip", to_utf(&output_path)))
                    .hint("")
                    .hint("Alternatively, you can overwrite this option by using the '--format' flag:")
                    .hint(format!(
                        "  ouch compress <FILES>... {} --format tar.gz",
                        to_utf(&output_path)
                    ));

                return Err(error.into());
            }

            if !formats.get(0).map(Extension::is_archive).unwrap_or(false) && represents_several_files(&files) {
                // This piece of code creates a suggestion for compressing multiple files
                // It says:
                // Change from file.bz.xz
                // To          file.tar.bz.xz
                let suggested_output_path = build_archive_file_suggestion(&output_path, ".tar")
                    .expect("output path did not contain a compression format");

                let output_path = to_utf(&output_path);

                let error = FinalError::with_title(format!("Cannot compress to '{}'.", output_path))
                    .detail("You are trying to compress multiple files.")
                    .detail(format!(
                        "The compression format '{}' cannot receive multiple files.",
                        &formats[0]
                    ))
                    .detail("The only supported formats that archive files into an archive are .tar and .zip.")
                    .hint(format!("Try inserting '.tar' or '.zip' before '{}'.", &formats[0]))
                    .hint(format!("From: {}", output_path))
                    .hint(format!("To:   {}", suggested_output_path));

                return Err(error.into());
            }

            if let Some(format) = formats.iter().skip(1).find(|format| format.is_archive()) {
                let error = FinalError::with_title(format!("Cannot compress to '{}'.", to_utf(&output_path)))
                    .detail(format!("Found the format '{}' in an incorrect position.", format))
                    .detail(format!(
                        "'{}' can only be used at the start of the file extension.",
                        format
                    ))
                    .hint(format!(
                        "If you wish to compress multiple files, start the extension with '{}'.",
                        format
                    ))
                    .hint(format!(
                        "Otherwise, remove the last '{}' from '{}'.",
                        format,
                        to_utf(&output_path)
                    ));

                return Err(error.into());
            }

            if output_path.exists() && !utils::user_wants_to_overwrite(&output_path, question_policy)? {
                // User does not want to overwrite this file, skip and return without any errors
                return Ok(());
            }

            let output_file = fs::File::create(&output_path)?;

            if !represents_several_files(&files) {
                // It is possible the file is already partially compressed so we don't want to compress it again
                // `ouch compress file.tar.gz file.tar.gz.xz` should produce `file.tar.gz.xz` and not `file.tar.gz.tar.gz.xz`
                let input_extensions = extension::extensions_from_path(&files[0]);

                // We calculate the formats that are left if we filter out a sublist at the start of what we have that's the same as the input formats
                let mut new_formats = Vec::with_capacity(formats.len());
                for (inp_ext, out_ext) in input_extensions.iter().zip(&formats) {
                    if inp_ext.compression_formats == out_ext.compression_formats {
                        new_formats.push(out_ext.clone());
                    } else if inp_ext
                        .compression_formats
                        .iter()
                        .zip(out_ext.compression_formats.iter())
                        .all(|(inp, out)| inp == out)
                    {
                        let new_ext = Extension::new(
                            &out_ext.compression_formats[..inp_ext.compression_formats.len()],
                            &out_ext.display_text,
                        );
                        new_formats.push(new_ext);
                        break;
                    }
                }
                // If the input is a sublist at the start of `formats` then remove the extensions
                // Note: If input_extensions is empty then it will make `formats` empty too, which we don't want
                if !input_extensions.is_empty() && new_formats != formats {
                    // Safety:
                    //   We checked above that input_extensions isn't empty, so files[0] has an extension.
                    //
                    //   Path::extension says: "if there is no file_name, then there is no extension".
                    //   Contrapositive statement: "if there is extension, then there is file_name".
                    info!(
                        accessible, // important information
                        "Partial compression detected. Compressing {} into {}",
                        to_utf(files[0].as_path().file_name().unwrap().as_ref()),
                        to_utf(&output_path)
                    );
                    formats = new_formats;
                }
            }
            let compress_result = compress_files(
                files,
                formats,
                output_file,
                &output_path,
                question_policy,
                file_visibility_policy,
            );

            if let Ok(true) = compress_result {
                // this is only printed once, so it doesn't result in much text. On the other hand,
                // having a final status message is important especially in an accessibility context
                // as screen readers may not read a commands exit code, making it hard to reason
                // about whether the command succeeded without such a message
                info!(accessible, "Successfully compressed '{}'.", to_utf(&output_path));
            } else {
                // If Ok(false) or Err() occurred, delete incomplete file
                // Print an extra alert message pointing out that we left a possibly
                // CORRUPTED FILE at `output_path`
                if let Err(err) = fs::remove_file(&output_path) {
                    eprintln!("{red}FATAL ERROR:\n", red = *colors::RED);
                    eprintln!("  Please manually delete '{}'.", to_utf(&output_path));
                    eprintln!(
                        "  Compression failed and we could not delete '{}'.",
                        to_utf(&output_path),
                    );
                    eprintln!(
                        "  Error:{reset} {}{red}.{reset}\n",
                        err,
                        reset = *colors::RESET,
                        red = *colors::RED
                    );
                }
            }

            compress_result?;
        }
        Subcommand::Decompress { files, output_dir } => {
            let mut output_paths = vec![];
            let mut formats = vec![];

            for path in files.iter() {
                let (file_output_path, file_formats) = extension::separate_known_extensions_from_name(path);
                output_paths.push(file_output_path);
                formats.push(file_formats);
            }

            if let ControlFlow::Break(_) = check_mime_type(&files, &mut formats, question_policy)? {
                return Ok(());
            }

            let files_missing_format: Vec<PathBuf> = files
                .iter()
                .zip(&formats)
                .filter(|(_, formats)| formats.is_empty())
                .map(|(input_path, _)| PathBuf::from(input_path))
                .collect();

            if !files_missing_format.is_empty() {
                let error = FinalError::with_title("Cannot decompress files without extensions")
                    .detail(format!(
                        "Files without supported extensions: {}",
                        pretty_format_list_of_paths(&files_missing_format)
                    ))
                    .detail("Decompression formats are detected automatically by the file extension")
                    .hint("Provide a file with a supported extension:")
                    .hint("  ouch decompress example.tar.gz")
                    .hint("")
                    .hint("Or overwrite this option with the '--format' flag:")
                    .hint(format!(
                        "  ouch decompress {} --format tar.gz",
                        to_utf(&files_missing_format[0])
                    ));

                return Err(error.into());
            }

            // The directory that will contain the output files
            // We default to the current directory if the user didn't specify an output directory with --dir
            let output_dir = if let Some(dir) = output_dir {
                if !utils::clear_path(&dir, question_policy)? {
                    // User doesn't want to overwrite
                    return Ok(());
                }
                utils::create_dir_if_non_existent(&dir)?;
                dir
            } else {
                PathBuf::from(".")
            };

            for ((input_path, formats), file_name) in files.iter().zip(formats).zip(output_paths) {
                let output_file_path = output_dir.join(file_name); // Path used by single file format archives
                decompress_file(input_path, formats, &output_dir, output_file_path, question_policy)?;
            }
        }
        Subcommand::List { archives: files, tree } => {
            let mut formats = vec![];

            for path in files.iter() {
                let (_, file_formats) = extension::separate_known_extensions_from_name(path);
                formats.push(file_formats);
            }

            if let ControlFlow::Break(_) = check_mime_type(&files, &mut formats, question_policy)? {
                return Ok(());
            }

            let not_archives: Vec<PathBuf> = files
                .iter()
                .zip(&formats)
                .filter(|(_, formats)| !formats.get(0).map(Extension::is_archive).unwrap_or(false))
                .map(|(path, _)| path.clone())
                .collect();

            if !not_archives.is_empty() {
                let error = FinalError::with_title("Cannot list archive contents")
                    .detail("Only archives can have their contents listed")
                    .detail(format!(
                        "Files are not archives: {}",
                        pretty_format_list_of_paths(&not_archives)
                    ));

                return Err(error.into());
            }

            let list_options = ListOptions { tree };

            for (i, (archive_path, formats)) in files.iter().zip(formats).enumerate() {
                if i > 0 {
                    println!();
                }
                let formats = flatten_compression_formats(&formats);
                list_archive_contents(archive_path, formats, list_options, question_policy)?;
            }
        }
    }
    Ok(())
}

fn check_mime_type(
    files: &[PathBuf],
    formats: &mut [Vec<Extension>],
    question_policy: QuestionPolicy,
) -> crate::Result<ControlFlow<()>> {
    for (path, format) in files.iter().zip(formats.iter_mut()) {
        if format.is_empty() {
            // File with no extension
            // Try to detect it automatically and prompt the user about it
            if let Some(detected_format) = try_infer_extension(path) {
                // Infering the file extension can have unpredicted consequences (e.g. the user just
                // mistyped, ...) which we should always inform the user about.
                info!(
                    accessible,
                    "Detected file: `{}` extension as `{}`",
                    path.display(),
                    detected_format
                );
                if user_wants_to_continue(path, question_policy, QuestionAction::Decompression)? {
                    format.push(detected_format);
                } else {
                    return Ok(ControlFlow::Break(()));
                }
            }
        } else if let Some(detected_format) = try_infer_extension(path) {
            // File ending with extension
            // Try to detect the extension and warn the user if it differs from the written one
            let outer_ext = format.iter().next_back().unwrap();
            if !outer_ext
                .compression_formats
                .ends_with(detected_format.compression_formats)
            {
                warning!(
                    "The file extension: `{}` differ from the detected extension: `{}`",
                    outer_ext,
                    detected_format
                );
                if !user_wants_to_continue(path, question_policy, QuestionAction::Decompression)? {
                    return Ok(ControlFlow::Break(()));
                }
            }
        } else {
            // NOTE: If this actually produces no false positives, we can upgrade it in the future
            // to a warning and ask the user if he wants to continue decompressing.
            info!(accessible, "Could not detect the extension of `{}`", path.display());
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn deduplicate_input_files(files: &mut Vec<PathBuf>, output_path: &Path) {
    let mut idx = 0;
    while idx < files.len() {
        if files[idx] == output_path {
            warning!(
                "The output file and the input file are the same: `{}`, skipping...",
                output_path.display()
            );
            files.remove(idx);
        } else {
            idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::build_archive_file_suggestion;

    #[test]
    fn builds_suggestion_correctly() {
        assert_eq!(build_archive_file_suggestion(Path::new("linux.png"), ".tar"), None);
        assert_eq!(
            build_archive_file_suggestion(Path::new("linux.xz.gz.zst"), ".tar").unwrap(),
            "linux.tar.xz.gz.zst"
        );
        assert_eq!(
            build_archive_file_suggestion(Path::new("linux.pkg.xz.gz.zst"), ".tar").unwrap(),
            "linux.pkg.tar.xz.gz.zst"
        );
        assert_eq!(
            build_archive_file_suggestion(Path::new("linux.pkg.zst"), ".tar").unwrap(),
            "linux.pkg.tar.zst"
        );
        assert_eq!(
            build_archive_file_suggestion(Path::new("linux.pkg.info.zst"), ".tar").unwrap(),
            "linux.pkg.info.tar.zst"
        );
    }
}
