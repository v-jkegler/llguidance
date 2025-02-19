use anyhow::Result;
use llguidance::{
    api::TopLevelGrammar, earley::XorShift, substring::chunk_into_words, TokenParser,
};
use sample_parser::*;

fn make_parser(lark: &str, quiet: bool) -> Result<TokenParser> {
    let grm = TopLevelGrammar::from_lark(lark.to_string());
    let mut parser = get_parser_factory().create_parser_ext2(
        grm,
        if quiet { 0 } else { 2 },
        if quiet { 1 } else { 2 },
    )?;
    parser.start_without_prompt();
    Ok(parser)
}

fn consume(parser: &mut TokenParser, tok: u32) {
    let n = parser.consume_token(tok).unwrap();
    assert!(n == 0);
}

fn lark_ok(lark: &str) {
    match make_parser(lark, false) {
        Err(e) => panic!("unexpected error: {}, grm:\n{}", e, lark),
        Ok(_) => {}
    }
}

fn lark_err_test(lark: &str, err: &str) {
    match make_parser(lark, false) {
        Err(e) => {
            let e = format!("{}", e);
            if !e.contains(err) {
                panic!(
                    "unexpected error: {}, expecting {:?}; grm:\n{}",
                    e, err, lark
                );
            }
        }
        Ok(_) => panic!("expected error: {}; grm:\n{}", err, lark),
    }
}

fn lark_str_test(lark: &str, should_accept: bool, s: &str, quiet: bool) {
    let trie = get_tok_env().tok_trie();
    let tokens = get_tok_env().tokenize(s);
    if !quiet {
        println!(
            "\n\ntokens: {}, accpt={}\ngrm:\n{}\n",
            trie.tokens_dbg(&tokens),
            should_accept,
            lark
        );
    }

    // let t0 = std::time::Instant::now();
    let mut p = make_parser(lark, quiet).unwrap();
    // println!("make_parser: {:?}", t0.elapsed());

    for tok in tokens.iter() {
        let m = p.compute_mask().unwrap();
        if m.is_allowed(*tok) {
            consume(&mut p, *tok);
        } else {
            if should_accept {
                panic!("unexpected token: {}", trie.token_dbg(*tok));
            }
            return;
        }
    }
    if p.is_accepting() {
        if !should_accept {
            panic!("unexpected accept");
        }
    } else {
        if should_accept {
            panic!("unexpected reject");
        }
    }
}

fn lark_str_test_many_ext(quiet: bool, lark: &str, passing: &[&str], failing: &[&str]) {
    for s in passing {
        lark_str_test(lark, true, s, quiet);
    }
    for s in failing {
        lark_str_test(lark, false, s, quiet);
    }
}

fn lark_str_test_many(lark: &str, passing: &[&str], failing: &[&str]) {
    lark_str_test_many_ext(false, lark, passing, failing);
}

fn lark_str_test_many_quiet(lark: &str, passing: &[&str], failing: &[&str]) {
    lark_str_test_many_ext(true, lark, passing, failing);
}

#[test]
fn test_dot_unicode() {
    lark_str_test_many(
        r#"start: /.../ "abc" /.../"#,
        &[
            "abcabcabc",
            "aaaabcccc",
            // NOTE: Also ensures that multi-byte characters still count as a single character
            "🔵🟠✅abc❌🟠🔵",
        ],
        &[
            "aaabcccc",
            "aaaaabcccc",
            "aaaabccc",
            "aaaabccccc",
            "🔵🟠✅❌abc❌✅🟠🔵",
            "🔵🟠abc🟠🔵",
        ],
    );
}

#[test]
fn test_lark_syntax_general() {
    lark_err_test(r#"root: "abc" "def""#, "no start");

    lark_err_test(
        r#"
            start: foo{7,6}
            foo: "a" | "b"
        "#,
        "range end must be >= start",
    );
    lark_err_test(
        r#"
            start: foo{-1,}
            foo: "a" | "b"
        "#,
        "range start must be >= 0",
    );
    lark_err_test(
        r#"
            start: foo{0,-1}
            foo: "a" | "b"
        "#,
        "range end must be >= start",
    );

    lark_err_test(
        r#"
            start: FOO
            FOO: ("a" | "b"){7,6}
        "#,
        "range end must be >= start",
    );
    lark_err_test(
        r#"
            start: FOO
            FOO: ("a" | "b"){-1,}
        "#,
        "range start must be >= 0",
    );
    lark_err_test(
        r#"
            start: FOO
            FOO: ("a" | "b"){0,-1}
        "#,
        "range end must be >= start",
    );

    lark_err_test(
        r#"
            start: FOO
            FOO: "a" | BAR
            BAR: "b" FOO
        "#,
        "circular reference in token",
    );

    lark_ok(
        r#"
            start: foo
            foo: "a" | bar
            bar: "b" foo
        "#,
    );

    lark_err_test(
        r#"
            start: FOO
            BAR: "b"
        "#,
        "unknown name",
    );

    lark_err_test(
        r#"
            start: foo
            bar: "b"
        "#,
        "unknown name",
    );

    lark_err_test(
        r#"
            start: BAR
            BAR: BAZ "a"
        "#,
        r#"token "BAZ" not found"#,
    );

    lark_ok(
        r#"
            %import common.INT
            start: INT
        "#,
    );
    lark_err_test(
        r#"
            %import common.BLAH
            start: BLAH
        "#,
        "Unknown common",
    );

    lark_err_test(r#" start: /[abc/ "#, "invalid regex");
    lark_ok(r#" start: /[abc]/ "#);
    lark_err_test(r#" start: /[abc]/l "#, "l-flag is not supported");

    lark_err_test(
        r#"
            start: FOO
            FOO: @1
        "#,
        "cannot be used in terminals",
    );
    lark_err_test(
        r#"
            start: FOO
            FOO: %json { }
        "#,
        "cannot be used in terminals",
    );
    lark_err_test(
        r#"
            start: FOO
            FOO: <[1234]>
        "#,
        "cannot be used in terminals",
    );
    lark_err_test(
        r#"
            start: FOO
            FOO: <|assistant|>
        "#,
        "cannot be used in terminals",
    );
    lark_err_test(
        r#"
            start: "A" | <|foobarbaz|>
        "#,
        "unknown special token",
    );

    lark_err_test(
        r#" start: "ab".."c" "#,
        "range start must be a single character",
    );
    lark_err_test(
        r#" start: "a".."cd" "#,
        "range end must be a single character",
    );
    lark_err_test(r#"  start: "d".."a" "#, "invalid range order");

    lark_err_test(r#"start: <[100-200-300]>"#, "invalid token range");
    lark_ok(r#"start: <[100-200,300-4002]>"#);
    lark_err_test(r#"start: <[100-200,100-200-300]>"#, "invalid token range");
    lark_err_test(r#"start: <[,]>"#, "empty token range");
    lark_err_test(r#"start: <[200-100]>"#, "invalid token range");
    lark_err_test(r#"start: <[200 - 100]>"#, "lexer error");

    lark_err_test(
        r#"
            start: foo
            foo: "a" | "b"
            foo: "c"
        "#,
        "duplicate rule",
    );
    lark_err_test(
        r#"
            start: FOO
            FOO: "a" | "b"
            FOO: "c"
        "#,
        "duplicate token",
    );
}

#[test]
fn test_lark_syntax_perc() {
    lark_err_test(r#"start: %json {"#, "EOF while parsing an object");
    lark_err_test(r#"start: %json { foo"#, "key must be a string");
    lark_err_test(r#"start: %json []"#, "failed to compile JSON schema");
    lark_err_test(
        r#"start: %json { "if": {} }"#,
        "failed to compile JSON schema",
    );

    lark_err_test(
        r#"
            %llguidance { "no_forcing": "yadda-dada"}
            start: "a" | "b"
        "#,
        "failed to parse %llguidance declaration",
    );

    lark_ok(r#" start: %regex { "substring_words": "foo bar" } "#);
    lark_ok(r#" start: %regex { "substring_chars": "foo bar" } "#);
    lark_ok(r#" start: %regex { "substring_chunks": ["foo", "bar"] } "#);

    lark_err_test(
        r#" start: %regex { "substring_words": true } "#,
        "failed to parse %regex",
    );

    lark_err_test(r#" start: %regex { "foobar": true } "#, "unknown field");

    lark_err_test(
        r#" start: %regex { "substring_words": "aa", "substring_chars": "bb" } "#,
        "only one field can be set on %regex",
    );

    lark_err_test(r#" start: %regex {  } "#, "no fields set on %regex");
}

#[test]
fn test_lark_syntax_attributes() {
    lark_ok(
        r#" start: foo
            foo[stop=""]: /.*/ "#,
    );

    lark_ok(
        r#" start: foo
            foo[stop="",max_tokens=12]: /.*/ "#,
    );

    lark_ok(
        r#" start: foo
            foo[capture,stop=""]: /.*/ "#,
    );

    lark_ok(
        r#" start: foo
            foo[capture="bar" , stop=""]: /.*/ "#,
    );

    lark_ok(
        r#" start: foo
            foo[stop = "foobar"]: /.*/ "#,
    );

    lark_ok(
        r#" start: foo
            foo[stop = /foobar/]: /.*/ "#,
    );

    lark_ok(
        r#" start: foo
            foo[stop = STOP]: /.*/
            STOP: "foobar"
        "#,
    );

    lark_err_test(
        r#" start: foo
            foo[foobar=12]: /.*/ "#,
        "Unknown attribute",
    );

    lark_err_test(
        r#" start: foo
            foo[stop=""="foo"]: /.*/ "#,
        "Expected token",
    );

    lark_err_test(
        r#" start: foo
            foo[max_tokens="foo"]: /.*/ "#,
        "Expected token",
    );
}

#[test]
fn test_repeat() {
    lark_str_test_many(
        r#"start:  ab{3,5}
           ab:  "a" | "b"
        "#,
        &["aba", "abaa", "aaaaa", "aabaa"],
        &["aa", "ab", "aaaaaa"],
    );

    lark_str_test_many(
        r#"start:  ab{3,}
           ab:  "a" | "b"
        "#,
        &["aba", "abaa", "aaaaa", "aabaa", "aaaaaa"],
        &["aa", "ab"],
    );

    lark_str_test_many(
        r#"start:  ab{,5}
           ab:  "a" | "b"
        "#,
        &["", "aa", "b", "aba", "abaa", "aaaaa", "aabaa"],
        &["aaaaaa"],
    );
}

#[test]
fn test_lexeme_substring_general() {
    for grm in &[
        r#" start: "A" %regex { "substring_words": "foo bar baz" } "B" "#,
        r#" start: SUB
            SUB: "A" %regex { "substring_words": "foo bar baz" } "B" "#,
    ] {
        lark_str_test_many(
            grm,
            &[
                "AfooB",
                "Abar bazB",
                "AbazB",
                "Afoo bar bazB",
                "Afoo bar B",
                "A bar bazB",
                "AB",
            ],
            &["Afoo bar baz", "AfoB"],
        );
    }

    lark_str_test_many(
        r#" start: "A" %regex { "substring_chunks": ["foo", " bar", " baz"] } "B" "#,
        &[
            "AfooB",
            "A bar bazB",
            "A bazB",
            "Afoo bar bazB",
            "Afoo barB",
            "AB",
            "A bar bazB",
        ],
        &["Afoo bar baz", "AfoB"],
    );
}

#[test]
fn test_lexeme_substring_chars_ascii() {
    lark_str_test_many(
        r#"start: %regex { "substring_chars": "The quick brown fox jumps over the lazy dog." }"#,
        &[
            "The quick brown fox jumps over the lazy dog.",
            "The quick brown fox",
            "he quick brow",
            "fox jump",
            "dog.",
        ],
        &["brown fx"],
    );
}

#[test]
fn test_lexeme_substring_chars_unicode() {
    lark_str_test_many(
        r#"start: %regex { "substring_chars": "빠른 갈색 여우가 게으른 개를 뛰어넘었다." }"#,
        &[
            "빠른 갈색 여우가 게으른 개를 뛰어넘었다.",
            "빠른 갈색 여우가 게으른",
            "른 갈색 여우",
            "여우가 게으",
            "뛰어넘었다.",
        ],
        &["갈색 여가"],
    );
}

#[test]
fn test_lexeme_substring_words_ascii() {
    lark_str_test_many(
        r#"start: %regex { "substring_words": "The quick brown fox jumps over the lazy dog." }"#,
        &[
            "The quick brown fox jumps over the lazy dog.",
            "The quick brown fox",
            "dog.",
        ],
        &["he quick brow", "fox jump", "brown fx"],
    );
}

#[test]
fn test_lexeme_substring_words_unicode() {
    lark_str_test_many(
        r#"start: %regex { "substring_words": "빠른 갈색 여우가 게으른 개를 뛰어넘었다." }"#,
        &[
            "빠른 갈색 여우가 게으른 개를 뛰어넘었다.",
            "빠른 갈색 여우가 게으른",
            "뛰어넘었다.",
        ],
        &["른 갈색 여우", "여우가 게으", "갈색 여가"],
    );
}

fn gen_words(seed: u32, num_words: usize) -> String {
    let letters = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789,.";
    let mut rnd = XorShift::new(seed + 1);
    let mut words = vec![];
    let num_words = rnd.from_range((num_words / 2)..num_words);
    for _ in 0..num_words {
        let mut word = String::new();
        let len = rnd.from_range(1..15);
        for _ in 0..len {
            let idx = rnd.from_range(0..letters.len());
            word.push(letters.as_bytes()[idx as usize] as char);
        }
        words.push(word);
    }
    words.join(" ")
}

fn quote_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

#[test]
fn test_large_select() {
    let num_words = 500;
    // it's kind of slow in non-release mode
    let num_opt = if cfg!(debug_assertions) { 100 } else { 1500 };

    let t0 = std::time::Instant::now();
    let mut grm_sz = 0;

    for start in &["start: OPTS\nOPTS: ", "start: opts\nopts: "] {
        let mut grm_head = start.to_string();
        let mut grm_tail = "".to_string();
        let options = (0..num_opt)
            .map(|i| gen_words(i, num_words))
            .collect::<Vec<_>>();
        for (i, opt) in options.iter().enumerate() {
            grm_head.push_str(&format!("OPT{} | ", i));
            grm_tail.push_str(&format!("OPT{}: {}\n", i, quote_str(opt)));
        }
        grm_head.push_str(" \"\"\n");
        let grm = format!("{}{}", grm_head, grm_tail);
        grm_sz = grm.len();

        lark_str_test_many_quiet(
            &grm,
            //&options.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            &[&options[2].as_str(), &options[7].as_str()],
            &["something that is unlikely to be in the options"],
        );
    }

    println!("large_select: {:?}; grm={}kB", t0.elapsed(), grm_sz / 1024);
}

#[test]
fn test_large_substring_words() {
    let words_str = gen_words(1, 5000);
    let words = chunk_into_words(&words_str);
    let grm = format!(
        "start: %regex {{ \"substring_words\": {} }}",
        quote_str(&words_str)
    );

    let mtch = words[50..100].to_vec().join("");
    let no_mtch = format!("{}{}", mtch, "XXX");
    lark_str_test_many_quiet(&grm, &[&mtch], &[&no_mtch]);
}

#[test]
fn test_large_substring_chars() {
    let chars = gen_words(2, 15000)[..10000].to_string();
    let grm = format!(
        "start: %regex {{ \"substring_chars\": {} }}",
        quote_str(&chars)
    );
    let mtch = chars[50..100].to_string();
    let no_mtch = format!("{}{}", mtch, "XXX");
    lark_str_test_many_quiet(&grm, &[&mtch], &[&no_mtch]);
}

#[test]
fn test_lexer_amb() {
    lark_str_test_many(
        r#"start: "'foo'" /a+/ | STRING /b+/
           STRING: /'[^']*'/
        "#,
        &["'foo'a", "'foo'aaa", "'bar'b", "'bar'bbb", "'foo'bb"],
        &["'bar'a", "'bar'c"],
    );
}
