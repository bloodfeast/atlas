//! IDEA-CAPTURE part 0: does this text propose, decide or constrain something, on
//! its own? No model, no network, no dependency, no heap: one pass over the bytes,
//! counting general English cues into a linear score. The weights are an input,
//! fitted by `eval.rs fit` from whichever label file it is given and never held
//! here. No phrase comes from a project's own vocabulary.

pub const N: usize = 28;

const BIAS: usize = 0;
const LN_WORDS: usize = 1;
const SHORT3: usize = 2;
const SHORT6: usize = 3;
const MODAL: usize = 4;
const TENTATIVE: usize = 5;
const WOULD: usize = 6;
const OPINION: usize = 7;
const WE_DO: usize = 8;
const WHAT_IF: usize = 9;
const CONSTRAIN: usize = 10;
const CAUSAL: usize = 11;
const COMPARE: usize = 12;
const STATE: usize = 13;
const FUNCTION: usize = 14;
const ENDS_ASK: usize = 15;
const STARTS_ASK: usize = 16;
const STARTS_ACK: usize = 17;
const STARTS_DO: usize = 18;
const STARTS_CHOICE: usize = 19;
const HAS_URL: usize = 20;
const PASTED: usize = 21;
const LN_SENTENCES: usize = 22;
const FUTURE: usize = 23;
const ASK_PROPOSE: usize = 24;
const RULE: usize = 25;
const NARRATE: usize = 26;
const LN_DIGITS: usize = 27;

const WORD_CAP: usize = 24;

/// One text's cue counts. Counted cues are `ln(1 + n)`, so a long text does not win
/// on length twice.
pub fn features(text: &str) -> [f32; N] {
    let b = text.as_bytes();
    let mut c = [0u32; N];
    let (mut word, mut prev) = ([0u8; WORD_CAP], [0u8; WORD_CAP]);
    let (mut len, mut prev_len) = (0usize, 0usize);
    let (mut words, mut in_paste, mut last) = (0u32, false, 0u8);
    let mut i = 0;
    while i <= b.len() {
        let ch = if i < b.len() { b[i] } else { b' ' };
        // An apostrophe joins (dont, id, lets); typed and curly read alike.
        if ch == b'\'' {
            i += 1;
            continue;
        }
        if b[i..].starts_with(&[0xE2, 0x80, 0x99]) {
            i += 3;
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch >= 0x80 {
            if len < WORD_CAP {
                word[len] = ch.to_ascii_lowercase();
            }
            len += 1;
            i += 1;
            continue;
        }
        if len > 0 {
            let w = &word[..len.min(WORD_CAP)];
            if !in_paste && w != b"redacted" {
                words += 1;
                on_word(w, &prev[..prev_len], words == 1, &mut c);
                prev = word;
                prev_len = len.min(WORD_CAP);
            }
            len = 0;
        }
        if ch == b'<' && i < b.len() {
            if b[i..].starts_with(b"<pasted_content") {
                in_paste = true;
                c[PASTED] = 1;
            } else if b[i..].starts_with(b"</pasted_content") {
                in_paste = false;
                // The closing tag's own words are not the user's.
                i += b[i..].iter().position(|&x| x == b'>').map_or(b.len() - i, |p| p + 1);
                continue;
            }
        }
        if !in_paste && i < b.len() {
            if !ch.is_ascii_whitespace() {
                last = ch;
            }
            if matches!(ch, b'.' | b'!' | b'?' | b';') && b.get(i + 1).is_none_or(u8::is_ascii_whitespace) {
                c[LN_SENTENCES] += 1;
            }
        }
        i += 1;
    }
    let mut f = [0f32; N];
    for k in 0..N {
        f[k] = (1.0 + c[k] as f32).ln();
    }
    f[BIAS] = 1.0;
    f[LN_WORDS] = (1.0 + words as f32).ln();
    f[SHORT3] = f32::from(u8::from(words <= 3));
    f[SHORT6] = f32::from(u8::from(words <= 6));
    f[ENDS_ASK] = f32::from(u8::from(last == b'?'));
    for k in [STARTS_ASK, STARTS_ACK, STARTS_DO, STARTS_CHOICE, HAS_URL, PASTED] {
        f[k] = f32::from(u8::from(c[k] > 0));
    }
    f
}

fn on_word(w: &[u8], prev: &[u8], first: bool, c: &mut [u32; N]) {
    match w {
        b"should" | b"shouldnt" | b"must" | b"mustnt" | b"need" | b"needs" | b"shall" | b"ought" | b"require" | b"requires" | b"want" | b"wants" => c[MODAL] += 1,
        b"could" | b"might" | b"may" | b"maybe" | b"perhaps" | b"possibly" | b"probably" | b"likely" | b"idea" | b"option" | b"consider" => c[TENTATIVE] += 1,
        b"would" | b"wouldnt" | b"id" => c[WOULD] += 1,
        b"never" | b"always" | b"only" | b"every" | b"without" | b"unless" | b"instead" | b"rather" | b"except" | b"default" | b"avoid" | b"dont" | b"cant" | b"wont" | b"not" | b"no" | b"keep" | b"limit" | b"at" => c[CONSTRAIN] += 1,
        b"because" | b"since" | b"so" | b"if" | b"then" | b"but" | b"though" | b"although" | b"when" | b"while" | b"as" | b"however" | b"otherwise" => c[CAUSAL] += 1,
        b"better" | b"worse" | b"cheaper" | b"faster" | b"slower" | b"more" | b"less" | b"efficient" | b"expensive" | b"cost" | b"tradeoff" | b"worth" | b"than" | b"best" | b"prefer" => c[COMPARE] += 1,
        b"is" | b"are" | b"be" | b"was" | b"were" | b"has" | b"have" | b"can" | b"its" | b"thats" | b"theres" => c[STATE] += 1,
        b"the" | b"a" | b"an" | b"of" | b"to" | b"in" | b"on" | b"for" | b"with" | b"from" | b"and" | b"or" => c[FUNCTION] += 1,
        b"will" | b"going" | b"later" | b"eventually" | b"future" => c[FUTURE] += 1,
        b"http" | b"https" => c[HAS_URL] += 1,
        _ => {}
    }
    match (prev, w) {
        (b"i", b"think" | b"feel" | b"guess" | b"believe" | b"would" | b"suppose" | b"want" | b"expect" | b"prefer" | b"wonder" | b"like" | b"was" | b"am")
        | (b"im", b"thinking" | b"wondering" | b"guessing" | b"concerned" | b"not")
        | (b"id", b"like" | b"say" | b"prefer" | b"rather")
        | (b"my", b"concern" | b"guess" | b"thought" | b"take" | b"opinion") => c[OPINION] += 1,
        (b"can" | b"could" | b"should" | b"shall", b"we") | (b"would", b"it") | (b"what" | b"how", b"would") => c[ASK_PROPOSE] += 1,
        (b"make", b"sure") | (b"has" | b"have", b"to") | (b"as", b"a") | (b"by", b"default") | (b"from", b"now") => c[RULE] += 1,
        (b"im", b"going" | b"gonna" | b"stepping" | b"using" | b"away") | (b"ill", b"be") | (b"i", b"have" | b"got" | b"added" | b"installed" | b"killed" | b"just" | b"see" | b"noticed") => c[NARRATE] += 1,
        (b"we", b"should" | b"could" | b"can" | b"need" | b"want" | b"will" | b"might" | b"must" | b"have") | (b"let", b"us") => c[WE_DO] += 1,
        (b"what", b"if") | (b"how", b"about") | (b"why", b"not") => c[WHAT_IF] += 1,
        _ => {}
    }
    if matches!(w, b"lets") {
        c[WE_DO] += 1;
    }
    if w.iter().any(u8::is_ascii_digit) {
        c[LN_DIGITS] += 1;
    }
    if first {
        match w {
            b"what" | b"when" | b"where" | b"why" | b"how" | b"who" | b"which" | b"do" | b"did" | b"does" | b"is" | b"are" | b"can" | b"any" | b"was" | b"were" | b"has" | b"have" => c[STARTS_ASK] = 1,
            b"ok" | b"okay" | b"done" | b"thanks" | b"thank" | b"great" | b"nice" | b"cool" | b"good" | b"yes" | b"yeah" | b"yep" | b"sure" | b"lol" | b"just" | b"hey" => c[STARTS_ACK] = 1,
            b"go" | b"run" | b"move" | b"add" | b"fix" | b"give" | b"skip" | b"start" | b"use" | b"check" | b"make" | b"merge" | b"commit" | b"push" | b"try" | b"stop" | b"continue" | b"proceed" | b"remove" | b"show" | b"read" | b"look" => c[STARTS_DO] = 1,
            _ => {}
        }
        if w.len() == 1 || w.iter().all(u8::is_ascii_digit) {
            c[STARTS_CHOICE] = 1;
        }
    }
}

/// The score of a text against a weight vector; the caller holds both the weights
/// and the threshold.
pub fn score_with(text: &str, weights: &[f32; N]) -> f32 {
    features(text).iter().zip(weights).map(|(f, w)| f * w).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apostrophes_join_and_cases_fold() {
        assert_eq!(features("LET'S go")[WE_DO], features("lets go")[WE_DO]);
        assert!(features("We’d never")[CONSTRAIN] > 0.0);
    }

    #[test]
    fn a_pasted_block_and_a_redaction_are_not_words() {
        let bare = features("we should keep it");
        assert_eq!(features("we should keep it [redacted]")[LN_WORDS], bare[LN_WORDS]);
        let pasted = features("we should keep it <pasted_content id=\"1\"> one two three </pasted_content>");
        assert_eq!(pasted[LN_WORDS], bare[LN_WORDS]);
        assert_eq!(pasted[PASTED], 1.0);
    }

    #[test]
    fn a_bare_reply_reads_short_and_a_question_reads_as_one() {
        let f = features("A");
        assert_eq!((f[SHORT3], f[STARTS_CHOICE]), (1.0, 1.0));
        let f = features("can we do this later?");
        assert_eq!(f[ENDS_ASK], 1.0);
        assert!(f[ASK_PROPOSE] > 0.0);
    }

    #[test]
    fn an_empty_text_and_a_long_word_do_not_panic() {
        assert_eq!(features("")[LN_WORDS], 0.0);
        let long = "x".repeat(10_000);
        assert_eq!(features(&long)[LN_WORDS], 2f32.ln());
    }
}
