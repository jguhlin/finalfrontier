use std::cmp;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use clap::{App, Arg, ArgMatches};
use finalfrontier::io::{thread_data_text, FileProgress, TrainInfo};
use finalfrontier::{
    CommonConfig, ModelType, SentenceIterator, SimpleVocab, SkipGramConfig, SkipgramTrainer,
    SubwordVocab, Vocab, VocabBuilder, WriteModelBinary, SGD,
};
use finalfusion::prelude::VocabWrap;
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use serde::Serialize;
use stdinout::OrExit;

use crate::subcommands::{show_progress, FinalfrontierApp, VocabConfig};

static CONTEXT: &str = "context";
static MODEL: &str = "model";

const PROGRESS_UPDATE_INTERVAL: u64 = 200;

/// Subcommand for training skipgram models.
pub struct SkipgramApp {
    train_info: TrainInfo,
    common_config: CommonConfig,
    skipgram_config: SkipGramConfig,
    vocab_config: VocabConfig,
}

impl SkipgramApp {
    /// Get the corpus path.
    pub fn corpus(&self) -> &str {
        &self.train_info.corpus()
    }

    /// Get the output path.
    pub fn output(&self) -> &str {
        &self.train_info.output()
    }

    /// Get the number of threads.
    pub fn n_threads(&self) -> usize {
        self.train_info.n_threads()
    }

    /// Get the common config.
    pub fn common_config(&self) -> CommonConfig {
        self.common_config
    }

    /// Get the skipgram config.
    pub fn skipgram_config(&self) -> SkipGramConfig {
        self.skipgram_config
    }

    /// Get the vocab config.
    pub fn vocab_config(&self) -> VocabConfig {
        self.vocab_config
    }

    /// Get the train information.
    pub fn train_info(&self) -> &TrainInfo {
        &self.train_info
    }

    fn skipgram_config_from_matches(matches: &ArgMatches) -> SkipGramConfig {
        let context_size = matches
            .value_of(CONTEXT)
            .map(|v| v.parse().or_exit("Cannot parse context size", 1))
            .unwrap();
        let model = matches
            .value_of(MODEL)
            .map(|v| ModelType::try_from_str(v).or_exit("Cannot parse model type", 1))
            .unwrap();

        SkipGramConfig {
            context_size,
            model,
        }
    }
}

impl FinalfrontierApp for SkipgramApp {
    fn app() -> App<'static, 'static> {
        Self::common_opts("skipgram")
            .about("Train a skip-gram model")
            .arg(
                Arg::with_name(CONTEXT)
                    .long("context")
                    .value_name("CONTEXT_SIZE")
                    .help("Context size")
                    .takes_value(true)
                    .default_value("10"),
            )
            .arg(
                Arg::with_name(MODEL)
                    .long(MODEL)
                    .value_name("MODEL")
                    .help("Model")
                    .takes_value(true)
                    .possible_values(&["dirgram", "skipgram", "structgram"])
                    .default_value("skipgram"),
            )
    }

    fn parse(matches: &ArgMatches) -> Self {
        let corpus = matches.value_of(Self::CORPUS).unwrap().into();
        let output = matches.value_of(Self::OUTPUT).unwrap().into();
        let n_threads = matches
            .value_of(Self::THREADS)
            .map(|v| v.parse().or_exit("Cannot parse number of threads", 1))
            .unwrap_or_else(|| cmp::min(num_cpus::get() / 2, 20));
        let train_info = TrainInfo::new(corpus, output, n_threads);
        SkipgramApp {
            train_info,
            common_config: Self::parse_common_config(&matches),
            skipgram_config: Self::skipgram_config_from_matches(&matches),
            vocab_config: Self::parse_vocab_config(&matches),
        }
    }

    fn run(&self) {
        match self.vocab_config() {
            VocabConfig::SubwordVocab(config) => {
                let vocab: SubwordVocab<_, _> = build_vocab(config, self.corpus());
                train(vocab, self);
            }
            VocabConfig::SimpleVocab(config) => {
                let vocab: SimpleVocab<String> = build_vocab(config, self.corpus());
                train(vocab, self);
            }
            VocabConfig::NGramVocab(config) => {
                let vocab: SubwordVocab<_, _> = build_vocab(config, self.corpus());
                train(vocab, self);
            }
        }
    }
}

fn train<V>(vocab: V, app: &SkipgramApp)
where
    V: Vocab<VocabType = String> + Into<VocabWrap> + Clone + Send + Sync + 'static,
    V::Config: Serialize,
    for<'a> &'a V::IdxType: IntoIterator<Item = u64>,
{
    let common_config = app.common_config();
    let n_threads = app.n_threads();
    let corpus = app.corpus();
    let mut output_writer = BufWriter::new(
        File::create(app.output()).or_exit("Cannot open output file for writing.", 1),
    );
    let trainer = SkipgramTrainer::new(
        vocab,
        XorShiftRng::from_entropy(),
        common_config,
        app.skipgram_config(),
    );
    let sgd = SGD::new(trainer.into());

    let mut children = Vec::with_capacity(n_threads);
    for thread in 0..n_threads {
        let corpus = corpus.to_owned();
        let sgd = sgd.clone();

        children.push(thread::spawn(move || {
            do_work(
                corpus,
                sgd,
                thread,
                n_threads,
                common_config.epochs,
                common_config.lr,
            );
        }));
    }

    show_progress(
        &common_config,
        &sgd,
        Duration::from_millis(PROGRESS_UPDATE_INTERVAL),
    );

    // Wait until all threads have finished.
    for child in children {
        let _ = child.join();
    }

    sgd.into_model()
        .write_model_binary(&mut output_writer, app.train_info().clone())
        .or_exit("Cannot write model", 1);
}

fn do_work<P, R, V>(
    corpus_path: P,
    mut sgd: SGD<SkipgramTrainer<R, V>>,
    thread: usize,
    n_threads: usize,
    epochs: u32,
    start_lr: f32,
) where
    P: Into<PathBuf>,
    R: Clone + Rng,
    V: Vocab<VocabType = String>,
    V::Config: Serialize,
    for<'a> &'a V::IdxType: IntoIterator<Item = u64>,
{
    let n_tokens = sgd.model().input_vocab().n_types();

    let f = File::open(corpus_path.into()).or_exit("Cannot open corpus for reading", 1);
    let (data, start) =
        thread_data_text(&f, thread, n_threads).or_exit("Could not get thread-specific data", 1);

    let mut sentences = SentenceIterator::new(&data[start..]);
    while sgd.n_tokens_processed() < epochs as usize * n_tokens {
        let sentence = if let Some(sentence) = sentences.next() {
            sentence
        } else {
            sentences = SentenceIterator::new(&*data);
            sentences
                .next()
                .or_exit("Iterator does not provide sentences", 1)
        }
        .or_exit("Cannot read sentence", 1);

        let lr = (1.0 - (sgd.n_tokens_processed() as f32 / (epochs as usize * n_tokens) as f32))
            * start_lr;

        sgd.update_sentence(&sentence, lr);
    }
}

fn build_vocab<P, V, C>(config: C, corpus_path: P) -> V
where
    P: AsRef<Path>,
    V: Vocab<VocabType = String> + From<VocabBuilder<C, String>>,
    VocabBuilder<C, String>: Into<V>,
{
    let f = File::open(corpus_path).or_exit("Cannot open corpus for reading", 1);
    let file_progress = FileProgress::new(f).or_exit("Cannot create progress bar", 1);

    let sentences = SentenceIterator::new(BufReader::new(file_progress));

    let mut builder = VocabBuilder::new(config);
    for sentence in sentences {
        let sentence = sentence.or_exit("Cannot read sentence", 1);

        for token in sentence {
            builder.count(token);
        }
    }

    builder.into()
}
