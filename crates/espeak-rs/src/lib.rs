use espeak_rs_sys;
use ffi_support::FfiStr;
use once_cell::sync::Lazy;
use regex::Regex;
use std::env;
use std::error::Error;
use std::ffi;
use std::fmt;
use std::path::PathBuf;

pub type ESpeakResult<T> = Result<T, ESpeakError>;

const CLAUSE_INTONATION_FULL_STOP: i32 = 0x00000000;
const CLAUSE_INTONATION_COMMA: i32 = 0x00001000;
const CLAUSE_INTONATION_QUESTION: i32 = 0x00002000;
const CLAUSE_INTONATION_EXCLAMATION: i32 = 0x00003000;
const CLAUSE_TYPE_SENTENCE: i32 = 0x00080000;
/// Name of the environment variable that points to the directory that contains `espeak-ng-data` directory
/// only needed if `espeak-ng-data` directory is not in the expected location (i.e. eSpeak-ng is not installed system wide)
const PIPER_ESPEAKNG_DATA_DIRECTORY: &str = "PIPER_ESPEAKNG_DATA_DIRECTORY";
const ESPEAKNG_DATA_DIR_NAME: &str = "espeak-ng-data";

#[derive(Debug, Clone)]
pub struct ESpeakError(pub String);

impl Error for ESpeakError {}

impl fmt::Display for ESpeakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "eSpeak-ng Error :{}", self.0)
    }
}

static LANG_SWITCH_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\([^)]*\)").unwrap());
static STRESS_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ˈˌ]").unwrap());
static ESPEAKNG_INIT: Lazy<ESpeakResult<()>> = Lazy::new(|| {
    let data_dir = match locate_espeak_data() {
        Ok(dir) => dir,
        Err(why) => return Err(ESpeakError(why)),
    };
    // Lives for the call: espeak copies the path.
    let Ok(data_dir) = ffi::CString::new(data_dir.display().to_string()) else {
        return Err(ESpeakError(format!(
            "the data directory {} has a NUL in its path",
            data_dir.display()
        )));
    };
    let es_data_path_ptr = data_dir.as_ptr();
    unsafe {
        let es_sample_rate = espeak_rs_sys::espeak_Initialize(
            espeak_rs_sys::espeak_AUDIO_OUTPUT_AUDIO_OUTPUT_RETRIEVAL,
            0,
            es_data_path_ptr,
            espeak_rs_sys::espeakINITIALIZE_DONT_EXIT as i32,
        );
        if es_sample_rate <= 0 {
            Err(ESpeakError(format!(
                "Failed to initialize eSpeak-ng. Try setting `{PIPER_ESPEAKNG_DATA_DIRECTORY}` environment variable to the directory that contains the `{ESPEAKNG_DATA_DIR_NAME}` directory. \
                Error code: `{es_sample_rate}`."
            )))
        } else {
            Ok(())
        }
    }
});

/// The directory holding `espeak-ng-data`, found at run time, never a path compiled in: the one
/// `PIPER_ESPEAKNG_DATA_DIRECTORY` names, else the data compiled into espeak-rs-sys, written once
/// under the user's cache ([`embedded_data_dir`]). No other copy is looked for (a working
/// directory's or an executable's `espeak-ng-data`, the build's own install directory): one left
/// from another build would change the phonemes with nothing said, and a build's directory is gone
/// once its target is (a seeded worktree, a copied build).
fn locate_espeak_data() -> Result<PathBuf, String> {
    if let Ok(dir) = env::var(PIPER_ESPEAKNG_DATA_DIRECTORY) {
        let p = PathBuf::from(dir);
        if p.join(ESPEAKNG_DATA_DIR_NAME).is_dir() {
            return Ok(p);
        }
        return Err(format!(
            "{PIPER_ESPEAKNG_DATA_DIRECTORY} is {}, which holds no `{ESPEAKNG_DATA_DIR_NAME}`",
            p.display()
        ));
    }
    let cache = cache_dir().ok_or(
        "no cache directory (XDG_CACHE_HOME, LOCALAPPDATA or HOME) to write espeak-ng-data into",
    )?;
    embedded_data_dir(&cache)
        .map_err(|e| format!("writing espeak-ng-data under {}: {e}", cache.display()))
}

/// `$XDG_CACHE_HOME`, else `%LOCALAPPDATA%`, else `~/.cache`.
fn cache_dir() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
}

/// The embedded `espeak-ng-data` written under `cache/espeak-rs/<content id>`, or found there
/// from an earlier run: written to a directory of its own and renamed into place, so a run that
/// dies midway leaves nothing another would take for whole. Writing it removes what earlier
/// writes left there: another id's data (a build of other data, never read again) and partial
/// directories whose writer is gone. The directory returned is the one holding `espeak-ng-data`.
fn embedded_data_dir(cache: &std::path::Path) -> std::io::Result<PathBuf> {
    use espeak_rs_sys::data::{ESPEAK_NG_DATA, ESPEAK_NG_DATA_ID};
    let root = cache.join("espeak-rs");
    let dir = root.join(ESPEAK_NG_DATA_ID);
    if dir.join(ESPEAKNG_DATA_DIR_NAME).is_dir() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&root)?;
    for entry in std::fs::read_dir(&root)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = match name.split_once(".partial-") {
            // A partial whose writer still runs is that writer's.
            Some((_, pid)) => !pid
                .parse::<u32>()
                .is_ok_and(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists()),
            None => name != ESPEAK_NG_DATA_ID,
        };
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let partial = root.join(format!(
        "{ESPEAK_NG_DATA_ID}.partial-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&partial);
    for (rel, bytes) in ESPEAK_NG_DATA {
        let file = partial.join(ESPEAKNG_DATA_DIR_NAME).join(rel);
        std::fs::create_dir_all(file.parent().expect("a file in a directory"))?;
        std::fs::write(&file, bytes)?;
    }
    match std::fs::rename(&partial, &dir) {
        Ok(()) => Ok(dir),
        // Another process put it there first: theirs is the same content.
        Err(_) if dir.join(ESPEAKNG_DATA_DIR_NAME).is_dir() => {
            let _ = std::fs::remove_dir_all(&partial);
            Ok(dir)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            Err(e)
        }
    }
}

pub fn text_to_phonemes(
    text: &str,
    language: &str,
    phoneme_separator: Option<char>,
    remove_lang_switch_flags: bool,
    remove_stress: bool,
) -> ESpeakResult<Vec<String>> {
    let mut phonemes = Vec::new();
    for line in text.lines() {
        phonemes.append(&mut _text_to_phonemes(
            line,
            language,
            phoneme_separator,
            remove_lang_switch_flags,
            remove_stress,
        )?)
    }
    Ok(phonemes)
}

pub fn _text_to_phonemes(
    text: &str,
    language: &str,
    phoneme_separator: Option<char>,
    remove_lang_switch_flags: bool,
    remove_stress: bool,
) -> ESpeakResult<Vec<String>> {
    if let Err(ref e) = Lazy::force(&ESPEAKNG_INIT) {
        return Err(e.clone());
    }
    // C strings that live for the call (espeak reads them; nothing here frees what it made).
    let language_c = ffi::CString::new(language)
        .map_err(|_| ESpeakError(format!("the voice name `{language}` has a NUL in it")))?;
    let set_voice_res = unsafe { espeak_rs_sys::espeak_SetVoiceByName(language_c.as_ptr()) };
    if set_voice_res != espeak_rs_sys::espeak_ERROR_EE_OK {
        return Err(ESpeakError(format!(
            "Failed to set eSpeak-ng voice to: `{}` ",
            language
        )));
    }
    let calculated_phoneme_mode = match phoneme_separator {
        Some(c) => ((c as u32) << 8u32) | espeak_rs_sys::espeakINITIALIZE_PHONEME_IPA,
        None => espeak_rs_sys::espeakINITIALIZE_PHONEME_IPA,
    };
    let phoneme_mode: i32 = calculated_phoneme_mode.try_into().unwrap();
    let mut sent_phonemes = Vec::new();
    let mut phonemes = String::new();
    let text_c =
        ffi::CString::new(text).map_err(|_| ESpeakError("the text has a NUL in it".to_string()))?;
    // espeak moves this pointer through `text_c`'s buffer, which outlives the loop.
    let mut text_c_char = text_c.as_ptr();
    let text_c_char_ptr = std::ptr::addr_of_mut!(text_c_char);
    let terminator: ffi::c_int = 0;
    while !text_c_char.is_null() {
        let ph_str = unsafe {
            let res = espeak_rs_sys::espeak_TextToPhonemes(
                text_c_char_ptr as _,
                espeak_rs_sys::espeakCHARS_UTF8.try_into().unwrap(),
                phoneme_mode,
            );
            FfiStr::from_raw(res)
        };
        phonemes.push_str(&ph_str.into_string());
        let intonation = terminator & 0x0000F000;
        if intonation == CLAUSE_INTONATION_FULL_STOP {
            phonemes.push('.');
        } else if intonation == CLAUSE_INTONATION_COMMA {
            phonemes.push(',');
        } else if intonation == CLAUSE_INTONATION_QUESTION {
            phonemes.push('?');
        } else if intonation == CLAUSE_INTONATION_EXCLAMATION {
            phonemes.push('!');
        }
        if (terminator & CLAUSE_TYPE_SENTENCE) == CLAUSE_TYPE_SENTENCE {
            sent_phonemes.push(std::mem::take(&mut phonemes));
        }
    }
    if !phonemes.is_empty() {
        sent_phonemes.push(std::mem::take(&mut phonemes));
    }
    if remove_lang_switch_flags {
        sent_phonemes = Vec::from_iter(
            sent_phonemes
                .into_iter()
                .map(|sent| LANG_SWITCH_PATTERN.replace_all(&sent, "").into_owned()),
        );
    }
    if remove_stress {
        sent_phonemes = Vec::from_iter(
            sent_phonemes
                .into_iter()
                .map(|sent| STRESS_PATTERN.replace_all(&sent, "").into_owned()),
        );
    }
    Ok(sent_phonemes)
}

// ==============================

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_ALICE: &str =
        "Who are you? said the Caterpillar. Replied Alice , rather shyly, I hardly know, sir!";

    #[test]
    fn test_basic_en() -> ESpeakResult<()> {
        let text = "test";
        let expected = "tˈɛst.";
        let phonemes = text_to_phonemes(text, "en-US", None, false, false)?.join("");
        assert_eq!(phonemes, expected);
        Ok(())
    }

    #[test]
    fn test_it_splits_sentences() -> ESpeakResult<()> {
        let phonemes = text_to_phonemes(TEXT_ALICE, "en-US", None, false, false)?;
        assert_eq!(phonemes.len(), 3);
        Ok(())
    }

    #[test]
    fn test_it_adds_phoneme_separator() -> ESpeakResult<()> {
        let text = "test";
        let expected = "t_ˈɛ_s_t.";
        let phonemes = text_to_phonemes(text, "en-US", Some('_'), false, false)
            .unwrap()
            .join("");
        assert_eq!(phonemes, expected);
        Ok(())
    }

    #[test]
    fn test_it_preserves_clause_breakers() -> ESpeakResult<()> {
        let phonemes = text_to_phonemes(TEXT_ALICE, "en-US", None, false, false)?.join("");
        let clause_breakers = ['.', ',', '?', '!'];
        for c in clause_breakers {
            assert_eq!(
                phonemes.contains(c),
                true,
                "Clause breaker `{}` not preserved",
                c
            );
        }
        Ok(())
    }

    #[test]
    fn test_arabic() -> ESpeakResult<()> {
        let text = "مَرْحَبَاً بِكَ أَيُّهَا الْرَّجُلْ";
        let expected = "mˈarħabˌaː bikˌa ʔaˈiːuhˌaː alrrˈadʒul.";
        let phonemes = text_to_phonemes(text, "ar", None, false, false)?.join("");
        assert_eq!(phonemes, expected);
        Ok(())
    }

    #[test]
    fn test_lang_switch_flags() -> ESpeakResult<()> {
        let text = "Hello معناها مرحباً";

        let with_lang_switch = text_to_phonemes(text, "ar", None, false, false)?.join("");
        assert_eq!(with_lang_switch.contains("(en)"), true);
        assert_eq!(with_lang_switch.contains("(ar)"), true);

        let without_lang_switch = text_to_phonemes(text, "ar", None, true, false)?.join("");
        assert_eq!(without_lang_switch.contains("(en)"), false);
        assert_eq!(without_lang_switch.contains("(ar)"), false);

        Ok(())
    }

    #[test]
    fn test_stress() -> ESpeakResult<()> {
        let stress_markers = ['ˈ', 'ˌ'];

        let with_stress = text_to_phonemes(TEXT_ALICE, "en-US", None, false, false)?.join("");
        assert_eq!(with_stress.contains(stress_markers), true);

        let without_stress = text_to_phonemes(TEXT_ALICE, "en-US", None, false, true)?.join("");
        assert_eq!(without_stress.contains(stress_markers), false);

        Ok(())
    }
    /// The embedded data is written once under the cache, named for its content, and is a data
    /// directory espeak-ng reads (its phoneme table there); a second call finds it.
    #[test]
    fn the_embedded_data_is_written_once_where_it_is_found() {
        let cache = env::temp_dir().join(format!("espeak-rs-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        // What earlier writes left: another build's data, a partial whose writer died, and one
        // whose writer (pid 1, always there) still runs.
        let root = cache.join("espeak-rs");
        for left in [
            "0123456789abcdef",
            "0123456789abcdef.partial-999999999",
            "x.partial-1",
        ] {
            std::fs::create_dir_all(root.join(left).join("espeak-ng-data")).unwrap();
        }
        let dir = embedded_data_dir(&cache).unwrap();
        assert!(
            !root.join("0123456789abcdef").exists(),
            "another id's data stays"
        );
        assert!(
            !root.join("0123456789abcdef.partial-999999999").exists(),
            "a dead partial stays"
        );
        assert!(
            root.join("x.partial-1").exists(),
            "a live writer's partial was taken"
        );
        std::fs::remove_dir_all(root.join("x.partial-1")).unwrap();
        assert!(dir.starts_with(cache.join("espeak-rs")));
        assert!(dir.join("espeak-ng-data/phontab").is_file());
        assert!(dir.join("espeak-ng-data/en_dict").is_file());
        let written = std::fs::metadata(dir.join("espeak-ng-data/phontab"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(embedded_data_dir(&cache).unwrap(), dir);
        let again = std::fs::metadata(dir.join("espeak-ng-data/phontab"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(written, again, "written once");
        let leftovers: Vec<_> = std::fs::read_dir(cache.join("espeak-rs"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn test_line_splitting() -> ESpeakResult<()> {
        let text = "Hello\nThere\nAnd\nWelcome";
        let phoneme_paragraphs = text_to_phonemes(text, "en-US", None, false, false)?;
        assert_eq!(phoneme_paragraphs.len(), 4);
        Ok(())
    }
}
