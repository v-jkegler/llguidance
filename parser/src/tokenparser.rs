use std::{sync::Arc, time::Duration};

use crate::{
    api::{GenGrammarOptions, ParserLimits, StopReason, TopLevelGrammar},
    earley::{
        grammars_from_json, BiasComputer, CGrammar, CSymIdx, DefaultBiasComputer, Parser,
        ParserStats,
    },
    infoln, warn, Logger,
};
use anyhow::Result;
use serde_json::json;
use toktrie::{InferenceCapabilities, SimpleVob, StepArg, StepResult, TokEnv, TokenId};

#[derive(Clone)]
pub struct TokenParser {
    pub token_env: TokEnv,
    pub parser: Parser,
    pub mid_process_start_time: instant::Instant,
    pub last_bias_time: Duration,
    pub inference_caps: InferenceCapabilities,
    pub logger: Logger,
    pub limits: ParserLimits,
    pub bias_computer: Arc<dyn BiasComputer>,
    pending_bogus_backtrack: u32,
    // sampling any of these will pop the parser stack:
    pop_tokens: Option<SimpleVob>,
    test_trace: bool,
    parser_stack: Vec<ParserStackEntry>,
    parser_llm_tokens_offset: usize,
    // this is empty for top-level parser,
    // and the previous grm_bytes for sub-parsers
    previous_grm_bytes: Vec<u8>,

    mid_process_was_accepting: bool,
    stop_reason: StopReason,
    error_message: Option<String>,

    no_bias_this_mid_process: bool,

    max_tokens_total: usize,
    max_tokens_parser: usize,
    compiled_grammars: Vec<Arc<CGrammar>>,

    // tokens currently in KV cache
    llm_tokens: Vec<TokenId>,
    llm_bytes: Vec<u8>,
    grm_prefix: Vec<u8>,
    is_fresh: bool,
}

#[derive(Clone)]
struct ParserStackEntry {
    parser: Parser,
    parser_llm_tokens_offset: usize,
    previous_grm_bytes_len: usize,
    symidx: CSymIdx,
    max_tokens_offset: usize,
    mask: Option<SimpleVob>,
    is_accepting: bool,
}

impl TokenParser {
    pub fn from_llguidance_json(
        token_env: TokEnv,
        top_grammar: TopLevelGrammar,
        mut logger: Logger,
        inference_caps: InferenceCapabilities,
        limits: ParserLimits,
        extra_lexemes: Vec<String>,
    ) -> Result<Self> {
        let mid_process_start_time = instant::Instant::now();
        let test_trace = top_grammar.test_trace;
        let max_tokens = top_grammar.max_tokens.unwrap_or(usize::MAX);
        let compiled_grammars = grammars_from_json(
            top_grammar,
            &token_env,
            &mut logger,
            limits.clone(),
            extra_lexemes,
        )?;
        let parser = Parser::new(
            Arc::clone(&compiled_grammars[0]),
            GenGrammarOptions::default(),
            limits.clone(),
        )?;

        Ok(TokenParser {
            bias_computer: Arc::new(DefaultBiasComputer::new(token_env.clone())),
            logger,
            test_trace,
            token_env,
            inference_caps,
            limits,
            pending_bogus_backtrack: 0,
            mid_process_start_time,
            mid_process_was_accepting: false,
            no_bias_this_mid_process: false,
            stop_reason: StopReason::NotStopped,
            error_message: None,
            pop_tokens: None,
            parser,
            parser_llm_tokens_offset: 0,
            parser_stack: Vec::new(),
            previous_grm_bytes: Vec::new(),
            compiled_grammars,
            llm_tokens: Vec::new(),
            llm_bytes: Vec::new(),
            grm_prefix: Vec::new(),
            max_tokens_total: max_tokens,
            max_tokens_parser: max_tokens,
            last_bias_time: Duration::from_secs(0),
            is_fresh: true,
        })
    }

    // regular .clone() uses a shared lexer state
    pub fn deep_clone(&self) -> Self {
        let mut copy = self.clone();
        copy.parser = self.parser.deep_clone();
        copy
    }

    pub fn stop_reason(&self) -> StopReason {
        self.stop_reason
    }

    pub fn is_fresh(&self) -> bool {
        self.is_fresh
    }

    pub fn parser_stats(&self) -> &ParserStats {
        self.parser.stats()
    }

    pub fn num_tokens(&self) -> usize {
        self.llm_tokens.len()
    }

    pub fn final_bytes(&self) -> &[u8] {
        &self.llm_bytes[self.grm_prefix.len()..]
    }

    pub fn mid_process_was_accepting(&self) -> bool {
        self.mid_process_was_accepting
    }

    pub fn bytes_since(&mut self, mut idx: usize) -> &[u8] {
        idx += self.grm_prefix.len();
        let endp = std::cmp::min(
            self.llm_bytes.len(),
            self.previous_grm_bytes
                .len()
                .saturating_add(self.parser.hidden_start()),
        );
        if idx >= self.llm_bytes.len() || idx >= endp {
            return &[];
        }
        &self.llm_bytes[idx..endp]
    }

    pub fn start_without_prompt(&mut self) {
        infoln!(
            self,
            "initial lexer cost: {} (no prompt)",
            self.parser.lexer_stats()
        );

        assert!(self.is_fresh);
        self.is_fresh = false;
    }

    pub fn process_prompt(&mut self, prompt: Vec<TokenId>) -> Vec<TokenId> {
        infoln!(self, "initial lexer cost: {}", self.parser.lexer_stats());

        assert!(self.is_fresh);
        self.is_fresh = false;

        assert!(self.llm_tokens.is_empty());

        let trie = self.token_env.tok_trie();
        infoln!(self, "prompt: {}", trie.tokens_dbg(&prompt));
        let mut prompt_bytes = trie.decode_raw(&prompt);
        self.parser.force_bytes();
        let grm_bytes = self.parser.get_bytes();
        prompt_bytes.extend_from_slice(&grm_bytes);
        let tokens = self.token_env.tokenize_bytes_prefix(&prompt_bytes);
        infoln!(self, "prompt+grm: {}", trie.tokens_dbg(&tokens));
        let (chop_tokens, chop_bytes) = self
            .parser
            .with_recognizer(|r| trie.chop_tokens(r, &tokens));
        let res_prompt = tokens[..tokens.len() - chop_tokens].to_vec();

        // if we moved a bunch of grammar to the prompt, update llm_tokens to reflect that
        if chop_bytes <= grm_bytes.len() {
            self.llm_bytes = grm_bytes[0..grm_bytes.len() - chop_bytes].to_vec();
            self.llm_tokens = self.token_env.tokenize_bytes_prefix(&self.llm_bytes);
            let decoded = self.token_env.tok_trie().decode_raw(&self.llm_tokens);
            if self.llm_bytes.len() > 0
                && decoded.len() > 0
                && &decoded[1..] == &self.llm_bytes
                && decoded[0] == b' '
            {
                infoln!(self, "applying <s>space hack");
                self.grm_prefix = decoded[0..1].to_vec();
                self.llm_bytes = decoded;
            }
            infoln!(self, "ini_tokens: {}", trie.tokens_dbg(&self.llm_tokens));
        } else {
            // pretend the final bit of prompt was the prefix of the grammar
            self.grm_prefix = prompt_bytes
                [prompt_bytes.len() - chop_bytes..prompt_bytes.len() - grm_bytes.len()]
                .to_vec();
            infoln!(
                self,
                "force_prefix: {:?}",
                String::from_utf8_lossy(&self.grm_prefix)
            );
        }

        infoln!(self, "res_prompt: {}", trie.tokens_dbg(&res_prompt));
        if self.test_trace {
            self.test_trace_json(&json!({
                "prompt": trie.test_trace_tokens(&prompt),
                "res_prompt": trie.test_trace_tokens(&res_prompt),
            }));
        }
        res_prompt
    }

    fn grm_bytes(&mut self) -> Vec<u8> {
        let mut grm_bytes = self.grm_prefix.clone();
        grm_bytes.extend_from_slice(&self.previous_grm_bytes);
        grm_bytes.extend_from_slice(&self.parser.get_bytes());
        grm_bytes
    }

    fn is_top_level_parser(&self) -> bool {
        self.parser_stack.is_empty()
    }

    fn test_trace_json(&mut self, j: &serde_json::Value) {
        if self.test_trace {
            infoln!(self, "TEST: {}", serde_json::to_string(j).unwrap());
        }
    }

    fn stop(&mut self, warn: &str, reason: StopReason) -> StepResult {
        if warn.len() > 0 {
            self.error_message = Some(warn.to_string());
            warn!(self, "{}; stopping", warn);
        }
        self.stop_reason = reason;
        StepResult::stop()
    }

    pub fn error_message(&self) -> Option<String> {
        self.error_message.clone()
    }

    // advance_parser() is a top-level method in this file.
    // This advance_parser() is called by Constraint::commit_token().
    // It is accessible via the advance_parser() method of
    // the LLInterpreter interface.
    //
    // The result here *never* includes a mask.
    // It's either stop or an unconditional splice (possibly noop).
    pub fn advance_parser(&mut self, arg: StepArg) -> StepResult {
        assert!(self.inference_caps.ff_tokens);
        assert!(!self.test_trace);

        self.no_bias_this_mid_process = true;
        let r = self.mid_process(arg);
        self.no_bias_this_mid_process = false;

        assert!(r.sample_mask.is_none());

        r
    }

    // mid_process() is a top-level method in this file.
    // mid_process() is called by Constraint::commit_token().
    // It is also be called by TokenParser::advance_parser()
    // within this file, in which case it is accessible
    // via the advance_parser() method of the LLInterpreter interface.
    pub fn mid_process(&mut self, mut arg: StepArg) -> StepResult {
        assert!(self.is_fresh == false, "process_prompt() not called");

        self.mid_process_start_time = instant::Instant::now();
        if self.stop_reason != StopReason::NotStopped {
            let trie = self.token_env.tok_trie();
            infoln!(
                self,
                "stopped; post tokens: bt={} {}",
                arg.backtrack,
                trie.tokens_dbg(&arg.tokens)
            );

            if arg.backtrack == 0
                && (arg.tokens.len() == 0
                    || (arg.tokens.len() == 1 && arg.tokens[0] == trie.eos_token()))
            {
                // Don't warn in this case
                return StepResult::stop();
            }

            warn!(self, "stopped ({})", self.stop_reason.to_string());
            return StepResult::stop();
        }
        if self.max_tokens_total == 0 {
            return self.stop("max_tokens_total reached", StopReason::MaxTokensTotal);
        }
        self.max_tokens_total -= 1;
        self.max_tokens_parser = self.max_tokens_parser.saturating_sub(1);

        if self.pending_bogus_backtrack != 0 {
            arg.backtrack = self.pending_bogus_backtrack;
            self.pending_bogus_backtrack = 0;
        }

        let trace = if self.test_trace {
            let tokens = self.token_env.tok_trie().test_trace_tokens(&arg.tokens);
            Some(json!({
                "backtrack": arg.backtrack,
                "tokens": tokens,
            }))
        } else {
            None
        };

        let r = self.mid_process_inner(arg);

        if self.test_trace {
            let res = if r.is_stop() {
                json!("stop")
            } else {
                let b = &r;
                json!({
                    "sample_mask": b.sample_mask.is_some(),
                    "temperature": b.temperature,
                    "splices": b.splices.iter().map(|s| {
                        json!({
                            "when_sampled": s.when_sampled,
                            "backtrack": s.backtrack,
                            "tokens": self.token_env.tok_trie().test_trace_tokens(&s.ff_tokens),
                        })
                    }).collect::<Vec<_>>(),
                })
            };
            self.test_trace_json(&json!({
                "arg": trace.unwrap(),
                "res": res,
            }));
        }

        if !self.inference_caps.ff_tokens && r.has_ff_tokens() {
            let spl = r.unconditional_splice().unwrap();
            assert!(spl.backtrack == 0);
            if spl.ff_tokens.len() == 0 {
                return self.stop(
                    "ff_tokens not allowed, but got empty splice",
                    StopReason::InternalError,
                );
            }

            let t = spl.ff_tokens[0];
            infoln!(self, "forcing ff_token by mask: {}", t);
            let mask = self.token_env.tok_trie().singleton_token_set(t);
            return StepResult::sample(mask, None);
        }

        r
    }

    fn mid_process_inner(&mut self, mut arg: StepArg) -> StepResult {
        let start_time = instant::Instant::now();

        self.mid_process_was_accepting = false;

        let trie = self.token_env.tok_trie();

        infoln!(
            self,
            "{}: bt={} {}",
            if self.no_bias_this_mid_process {
                "commit_token"
            } else {
                "compute_mask"
            },
            arg.backtrack,
            trie.tokens_dbg(&arg.tokens)
        );

        if arg.tokens.len() == 1 {
            if let Some(pop) = &self.pop_tokens {
                if pop.is_allowed(arg.tokens[0]) {
                    infoln!(self, "pop_tokens hit: {}", trie.token_set_dbg(pop));
                    let pentry = self.parser_stack.last().unwrap();
                    // if the top of parse stack allows this token, we should stop
                    // popping parsers in the next iteration - clear pop_tokens
                    if pentry.mask.as_ref().unwrap().is_allowed(arg.tokens[0]) {
                        self.pop_tokens = None;
                    }
                    self.pop_parser();
                    return self.mid_process_inner(arg);
                }
            }
        }
        self.pop_tokens = None;

        let mut has_eos = false;

        if arg.tokens.contains(&trie.eos_token()) {
            assert!(arg.tokens.len() == 1);
            if self.parser.scan_eos() {
                // it got scanned correctly, so we remove it
                infoln!(self, "scanned eos_token");
                arg.tokens.clear();
            } else {
                infoln!(self, "didn't scan eos_token; saving");
                arg.save_tokens(&mut self.llm_tokens);
                has_eos = true;
            }
        } else {
            arg.save_tokens(&mut self.llm_tokens);
        }

        let new_bytes = trie.decode_raw(&arg.tokens);
        self.llm_bytes.extend_from_slice(&new_bytes);

        // eprintln!(
        //     "llm_bytes: {:?}\nllm_tokens: {}\n{:?}",
        //     String::from_utf8_lossy(&self.llm_bytes),
        //     trie.tokens_dbg(&self.llm_tokens),
        //     self.llm_tokens
        // );

        // TODO maybe remove in future
        if self.llm_bytes != trie.decode_raw(&self.llm_tokens) {
            panic!(
                "llm_bytes mismatch:\n    {:?}\n    {:?}",
                String::from_utf8_lossy(&self.llm_bytes),
                String::from_utf8_lossy(&trie.decode_raw(&self.llm_tokens))
            );
        }

        match self.parser.apply_tokens(
            trie,
            &self.llm_tokens[self.parser_llm_tokens_offset..],
            if self.is_top_level_parser() {
                self.grm_prefix.len()
            } else {
                0
            },
        ) {
            Ok("") => {}
            Ok(msg) => infoln!(self, "parser: {}", msg),
            Err(e) => {
                return self.stop(
                    &format!("Parser Error: {}", e),
                    StopReason::ParserTooComplex,
                );
            }
        };

        self.parser.filter_max_tokens();

        // force after scanning tokens from LLM (this may walk the parser some more)
        self.parser.force_bytes();
        let grm_bytes = self.grm_bytes();
        let trie = self.token_env.tok_trie(); // make borrow-checker happy

        let mut backtrack = 0;

        // println!(
        //     "\nllm_bytes: {:?}\ngrm_bytes: {:?}\n",
        //     String::from_utf8_lossy(&self.llm_bytes),
        //     String::from_utf8_lossy(&grm_bytes),
        // );

        // now, see if we need to backtrack
        if self.llm_bytes.len() > grm_bytes.len()
            || self.llm_bytes != grm_bytes[0..self.llm_bytes.len()]
        {
            let mut ptr = 0;
            for (idx, t) in self.llm_tokens.iter().enumerate() {
                let b = trie.token(*t);
                let pend = ptr + b.len();
                if pend > grm_bytes.len() || b != &grm_bytes[ptr..pend] {
                    backtrack = self.llm_tokens.len() - idx;
                    infoln!(
                        self,
                        "backtrack: {} (deletes: {:?})",
                        backtrack,
                        String::from_utf8_lossy(&self.llm_bytes[ptr..])
                    );
                    assert!(backtrack > 0);
                    self.llm_bytes.drain(ptr..);
                    break;
                }
                ptr = pend;
            }
            if backtrack == 0 {
                panic!(
                    "backtrack failed {:?} {:?}",
                    String::from_utf8_lossy(&self.llm_bytes),
                    String::from_utf8_lossy(&grm_bytes)
                );
            }
        }

        // if arg.tokens.contains(&trie.eos_token()) {
        //     return StepResult::stop();
        // }

        let new_forced = grm_bytes[self.llm_bytes.len()..].to_vec();
        let mut token_prefix = Vec::new();

        let do_force = new_forced.len() > 0 && !self.parser.grammar().lexer_spec().no_forcing;

        if do_force || backtrack > 0 {
            let mut grm_tokens = self.token_env.tokenize_bytes_prefix(&new_forced);
            infoln!(
                self,
                "forced: {} bytes:{:?} tokens:{:?}",
                trie.tokens_dbg(&grm_tokens),
                new_forced,
                grm_tokens
            );
            let (chop_tokens, chop_bytes) = self
                .parser
                .with_recognizer(|r| trie.chop_tokens(r, &grm_tokens));
            infoln!(self, "chop: {} tokens, {} bytes", chop_tokens, chop_bytes);
            token_prefix = new_forced[new_forced.len() - chop_bytes..].to_vec();
            // here we remove a suffix from grm_tokens that could be possibly tokenized differently
            grm_tokens.truncate(grm_tokens.len() - chop_tokens);

            if grm_tokens.len() > 0 || backtrack > 0 {
                infoln!(
                    self,
                    "fixed_tokens: {} bt={}",
                    trie.tokens_dbg(&grm_tokens),
                    backtrack
                );
                if backtrack != 0 && !self.inference_caps.backtrack {
                    let msg = format!(
                        "can't backtrack over {}; this may confuse the model",
                        trie.tokens_dbg(
                            &self.llm_tokens[self.llm_tokens.len() - backtrack as usize..]
                        )
                    );
                    warn!(self, "{}", msg);
                    // Setting error is too extreme here (may terminate the sequence)
                    // self.error_message = Some(msg);
                    self.pending_bogus_backtrack = backtrack as u32;
                    backtrack = 0;
                }
                return StepResult::splice(backtrack as u32, grm_tokens);
            } else {
                infoln!(self, "no fixed tokens");
            }
        } else if new_forced.len() > 0 {
            token_prefix.extend_from_slice(&new_forced);
            infoln!(self, "no-forced bytes:{:?}", new_forced);
        }

        if token_prefix.is_empty() {
            if let Err(e) = self.maybe_push_parser() {
                return self.stop(
                    &format!("Error creating nested parser: {}", e),
                    StopReason::InternalError,
                );
            }
        }

        let (inner_done, inner_accepting) = {
            let empty_token_prefix = token_prefix.is_empty();
            let lexer_bytes = self.parser.has_pending_lexeme_bytes();
            let is_accepting = self.parser.is_accepting();
            let can_advance = self.parser.can_advance();
            let inner_done = empty_token_prefix && is_accepting && (!can_advance || has_eos);
            infoln!(
                self,
                "inner_done: {inner_done}; lexer_bytes: {lexer_bytes}; \
                can_advance: {can_advance} (eos:{has_eos}); \
                accept: {is_accepting}; \
                empty_token_prefix: {empty_token_prefix}"
            );
            let inner_accepting = is_accepting && empty_token_prefix;
            (inner_done, inner_accepting)
        };

        let trie = self.token_env.tok_trie();

        if inner_done || self.max_tokens_parser == 0 {
            if self.parser_stack.is_empty() {
                self.mid_process_was_accepting = inner_accepting;
                infoln!(
                    self,
                    "only eos token allowed, stopping; accepting: {}",
                    inner_accepting
                );
                return self.stop(
                    "",
                    if inner_done {
                        if has_eos {
                            StopReason::EndOfSentence
                        } else {
                            StopReason::NoExtension
                        }
                    } else {
                        StopReason::MaxTokensParser
                    },
                );
            } else {
                infoln!(self, "pop_parser; tokens left {}", self.max_tokens_parser);
                self.pop_parser();
                // re-start the whole process with a nice tail-recursion
                return self.mid_process_inner(if has_eos {
                    arg
                } else {
                    StepArg {
                        backtrack: 0,
                        tokens: Vec::new(),
                        sampled: None,
                    }
                });
            }
        }

        if self.no_bias_this_mid_process {
            self.no_bias_this_mid_process = false;
            return StepResult::noop();
        }

        let pre = instant::Instant::now();
        let pre_stats = self.parser.stats().clone();
        let mut set = self
            .parser
            .compute_bias(&*self.bias_computer, &token_prefix);
        let p_stats = self.parser.stats().delta(&pre_stats);
        if let Some(err) = self.parser.lexer_error() {
            let err = format!("lexer error: {}", err);
            return self.stop(&err, StopReason::LexerTooComplex);
        }
        self.last_bias_time = pre.elapsed();

        if inner_accepting {
            let mut all_accepting = true;
            if self.parser_stack.len() > 0 {
                let mut pop_tokens = trie.alloc_token_set();
                for pentry in self.parser_stack.iter_mut() {
                    if pentry.mask.is_none() {
                        assert!(token_prefix.is_empty());
                        let (is_accepting, mask) = pentry
                            .parser
                            .compute_bias_after_gen_grammar(&*self.bias_computer, pentry.symidx);
                        if let Some(err) = pentry.parser.lexer_error() {
                            let err = format!("lexer error (inner): {}", err);
                            return self.stop(&err, StopReason::LexerTooComplex);
                        }
                        infoln!(self, "bias for upper parser: {}", trie.token_set_dbg(&mask));
                        pentry.mask = Some(mask);
                        pentry.is_accepting = is_accepting;
                    }
                    let m = pentry.mask.as_ref().unwrap();
                    pop_tokens.or_minus(m, &set);
                    set.or(m);
                    if !pentry.is_accepting {
                        all_accepting = false;
                        break;
                    }
                }
                infoln!(self, "pop_tokens: {}", trie.token_set_dbg(&pop_tokens));
                self.pop_tokens = Some(pop_tokens);
            }
            self.mid_process_was_accepting = all_accepting;
            if all_accepting {
                set.allow_token(trie.eos_token());
            }
        }

        infoln!(
            self,
            "step-stats: {:?}; {} lex fuel; {}",
            start_time.elapsed(),
            p_stats.lexer_cost,
            self.parser.lexer_stats(),
        );

        infoln!(
            self,
            "bias: (pref: {:?}; accpt: {}) {}",
            String::from_utf8_lossy(&token_prefix),
            self.mid_process_was_accepting,
            self.token_env.tok_trie().token_set_dbg(&set)
        );

        if set.num_set() == 0 {
            infoln!(self, "no tokens allowed, stopping");
            return self.stop("", StopReason::NoExtensionBias);
        }

        return StepResult::sample(set, self.parser.temperature());
    }

    fn maybe_push_parser(&mut self) -> Result<()> {
        if let Some((msg, symidx, gen_grammar)) = self.parser.maybe_gen_grammar() {
            if msg.len() > 0 {
                warn!(self, "{}", msg);
            }
            let grm = Arc::clone(&self.compiled_grammars[gen_grammar.grammar.0]);
            let max_tokens = self.parser.grammar().sym_data(symidx).props.max_tokens;
            let parser = Parser::new(grm, gen_grammar, self.limits.clone())?;
            let mut old_parser = std::mem::replace(&mut self.parser, parser);
            self.parser.take_global_state_from(&mut old_parser);
            let mut entry = ParserStackEntry {
                parser: old_parser,
                parser_llm_tokens_offset: self.parser_llm_tokens_offset,
                previous_grm_bytes_len: self.previous_grm_bytes.len(),
                symidx,
                max_tokens_offset: self.max_tokens_total.saturating_sub(self.max_tokens_parser),
                mask: None,
                is_accepting: false, // computed with mask
            };
            self.max_tokens_parser = std::cmp::min(self.max_tokens_parser, max_tokens);
            self.parser_llm_tokens_offset = self.llm_tokens.len();
            self.previous_grm_bytes
                .extend_from_slice(&entry.parser.get_bytes());
            self.parser_stack.push(entry);
        }
        Ok(())
    }

    fn pop_parser(&mut self) {
        let inner_bytes = self.parser.get_bytes();
        let entry = self.parser_stack.pop().unwrap();
        let mut prev_parser = std::mem::replace(&mut self.parser, entry.parser);
        self.parser.take_global_state_from(&mut prev_parser);
        self.parser_llm_tokens_offset = entry.parser_llm_tokens_offset;
        self.previous_grm_bytes
            .truncate(entry.previous_grm_bytes_len);
        infoln!(
            self,
            "pop_parser: {} tokens left; new {} - {} = {}",
            self.max_tokens_parser,
            self.max_tokens_total,
            entry.max_tokens_offset,
            self.max_tokens_total
                .saturating_sub(entry.max_tokens_offset)
        );
        self.max_tokens_parser = self
            .max_tokens_total
            .saturating_sub(entry.max_tokens_offset);
        self.parser.scan_gen_grammar(entry.symidx, inner_bytes);
    }
}
