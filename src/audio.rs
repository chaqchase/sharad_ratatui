use crate::{
    ai::GameAI,
    assistant::ASSETS_DIR,
    error::{AIError, AudioError, Error, Result},
    message::{AIMessage, Fluff},
    save::get_game_data_dir,
};
use async_openai::{
    Audio, Client,
    config::OpenAIConfig,
    types::{CreateSpeechRequestArgs, CreateTranscriptionRequestArgs, SpeechModel, Voice},
};
use chrono::Local;
use cpal::{
    FromSample, Sample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures::{StreamExt, stream::FuturesOrdered};
use include_dir::{Dir, DirEntry, File as iFile};
use rodio::{Decoder, OutputStream, Sink, Source};
use std::{
    fs::{self, File, create_dir_all},
    io::{BufReader, BufWriter, Cursor},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::sleep,
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum AudioNarration {
    Generating(GameAI, Fluff, PathBuf),
    Playing(Fluff),
    // Paused,
    Stopped,
}

impl AudioNarration {
    pub fn handle_audio(
        &mut self,
        ai_sender: tokio::sync::mpsc::UnboundedSender<AIMessage>,
    ) -> Result<()> {
        match &self {
            AudioNarration::Generating(game_ai, fluff, save_path) => {
                log::info!("AudioNarration::Generating: {fluff:#?}");
                self.generate_narration(
                    game_ai.client.clone(),
                    fluff.clone(),
                    save_path.clone(),
                    ai_sender,
                )?;
            }
            AudioNarration::Playing(fluff) => {
                log::info!("AudioNarration::Playing: {fluff:#?}");
                let fluff = fluff.clone();
                tokio::spawn(async move {
                    for file in fluff.dialogue.iter() {
                        if let Some(audio_path) = &file.audio {
                            if let Err(e) = play_audio(audio_path.clone()) {
                                log::error!("Failed to read audio: {e:#?}");
                            };
                        }
                    }
                });
            }
            // AudioNarration::Paused => todo!("Need to handle the Paused AudioNarration"),
            AudioNarration::Stopped => {}
        }
        Ok(())
    }

    fn generate_narration(
        &mut self,
        client: async_openai::Client<OpenAIConfig>,
        mut fluff: Fluff,
        save_path: PathBuf,
        ai_sender: tokio::sync::mpsc::UnboundedSender<AIMessage>,
    ) -> Result<()> {
        tokio::spawn(async move {
            fluff
                .speakers
                .iter_mut()
                .for_each(|speaker| speaker.assign_voice());

            log::info!("Before audio generation: {fluff:#?}");

            let mut audio_futures = FuturesOrdered::new();

            for (index, fluff_line) in fluff.dialogue.iter_mut().enumerate() {
                let voice = fluff
                    .speakers
                    .iter()
                    .find(|s| s.index == fluff_line.speaker_index)
                    .and_then(|s| s.voice.clone())
                    .expect("Voice not found for speaker");

                let text = fluff_line.text.clone();
                let save_path = save_path.clone();
                let client = client.clone();

                // Generate the audio concurrently, keeping track of the index
                audio_futures.push_back(async move {
                    let result = generate_audio(&client, &save_path, &text, voice).await;
                    (result, index)
                });
            }

            log::info!("generate_audio done");
            // Process the results in order
            while let Some((result, index)) = audio_futures.next().await {
                log::info!("generate_audio results: {result:#?}");
                if let Ok(path) = result {
                    fluff.dialogue[index].audio = Some(path);
                }
            }

            log::info!("After audio generation: {fluff:#?}");
            if let Err(e) =
                ai_sender.send(AIMessage::AudioNarration(AudioNarration::Playing(fluff)))
            {
                panic!("Err sending AudioNarration: {}", e)
            };
        });
        Ok(())
    }
}

pub async fn generate_audio(
    client: &async_openai::Client<OpenAIConfig>,
    save_path: &Path,
    text: &str,
    voice: Voice,
) -> Result<PathBuf> {
    let audio = Audio::new(client);

    let response = audio
        .speech(
            CreateSpeechRequestArgs::default()
                .input(text)
                .voice(voice)
                .model(SpeechModel::Tts1)
                .speed(1.3)
                .build()
                .map_err(AIError::OpenAI)?,
        )
        .await
        .map_err(AIError::OpenAI)?;

    let logs_dir = save_path
        .parent()
        .expect("Expected the save_path parent dir")
        .join("logs");

    log::info!("generate_audio logs_dir: {logs_dir:#?}");
    fs::create_dir_all(&logs_dir).map_err(AIError::Io)?;

    let uuid = Uuid::new_v4();
    let file_name = format!("{}_{}.mp3", Local::now().format("%Y-%m-%d_%H:%M:%S"), uuid);
    let file_path = logs_dir.join(file_name);
    response
        .save(file_path.to_str().expect("Expected a String"))
        .await
        .map_err(AIError::OpenAI)?;

    Ok(file_path)
}

pub fn try_play_asset(sound_name: &str) -> Result<()> {
    if let Some(path) = get_sound_asset_path(sound_name) {
        tokio::spawn(async move {
            if let Err(e) = play_asset(path) {
                log::error!("Failed to play alert sound: {e:#?}");
            }
        });
        Ok(())
    } else {
        let e = format!(
            "Sound {sound_name} not found in {:#?}",
            ASSETS_DIR.entries()
        );
        log::error!("{e}");
        Err(e.into())
    }
}

pub fn play_asset(file_path: PathBuf) -> Result<()> {
    log::info!("Playing asset: {file_path:#?}");
    let (_stream, stream_handle) =
        OutputStream::try_default().expect("Failed to get output stream");
    let sink = Sink::try_new(&stream_handle).expect("Failed to create audio sink");

    if let Some(file) = ASSETS_DIR.get_file(&file_path) {
        let cursor = Cursor::new(file.contents());
        let source = Decoder::new(cursor).map_err(AudioError::Decode)?;

        sink.append(source);
        sink.sleep_until_end();
    };
    log::info!("End of play_asset");
    Ok(())
}

// HACK: Still need an interruption method
pub fn play_audio(file_path: PathBuf) -> Result<()> {
    log::info!("Playing audio: {file_path:#?}");
    let (_stream, stream_handle) =
        OutputStream::try_default().expect("Failed to get output stream");
    let sink = Sink::try_new(&stream_handle).expect("Failed to create audio sink");

    if let Ok(file) = File::open(&file_path) {
        let source = Decoder::new(BufReader::new(file)).expect("Failed to decode audio");
        sink.append(source);
        sink.sleep_until_end();
        sleep(Duration::from_millis(100));
        log::info!("End of play_audio");
    };

    Ok(())
}

#[derive(Debug, Clone)]
pub enum AudioDir {
    GameDir(PathBuf),
    TempDir(PathBuf),
}

impl TryFrom<Option<PathBuf>> for AudioDir {
    fn try_from(path: Option<PathBuf>) -> Result<Self> {
        match path {
            Some(p) => {
                let save_path_parent = p.parent().expect("Expected a save_path");
                let logs_path = save_path_parent.join("logs");
                create_dir_all(&logs_path)?;
                Ok(AudioDir::GameDir(logs_path))
            }
            None => {
                let logs_path = get_game_data_dir().join("temp_logs");
                create_dir_all(&logs_path)?;
                Ok(AudioDir::TempDir(logs_path))
            }
        }
    }

    type Error = Error;
}

#[derive(Debug)]
pub struct Transcription {
    is_recording: Arc<AtomicBool>,
    client: Client<OpenAIConfig>,
    dir: AudioDir,
    recording_path: Option<PathBuf>,
    sender: UnboundedSender<String>,
    path: Arc<Mutex<Option<PathBuf>>>,
    pub transcription: String,
}

impl Clone for Transcription {
    fn clone(&self) -> Self {
        Self {
            is_recording: self.is_recording.clone(),
            client: self.client.clone(),
            dir: self.dir.clone(),
            recording_path: self.recording_path.clone(),
            sender: self.sender.clone(),
            path: self.path.clone(),
            transcription: self.transcription.clone(),
        }
    }
}

impl Transcription {
    pub fn new(
        path: Option<PathBuf>,
        client: Client<OpenAIConfig>,
    ) -> Result<(UnboundedReceiver<String>, Transcription)> {
        let (t_sender, t_receiver) = unbounded_channel();
        let mut transcription = Self {
            is_recording: Arc::new(AtomicBool::new(true)),
            client,
            dir: AudioDir::try_from(path)?,
            recording_path: None,
            sender: t_sender,
            path: Arc::new(Mutex::new(None)),
            transcription: String::new(),
        };

        transcription.start_recording();
        Ok((t_receiver, transcription))
    }

    pub fn start_recording(&mut self) {
        let dir = self.dir.clone();
        let is_recording = self.is_recording.clone();
        let future_path = Arc::clone(&self.path);

        thread::spawn(move || match record_audio(dir, is_recording) {
            Ok(path) => {
                *future_path.lock().unwrap() = Some(path);
            }
            Err(e) => log::error!("Error recording audio: {:?}", e),
        });
    }

    fn stop(&mut self) {
        self.is_recording.fetch_not(Ordering::SeqCst);
    }

    pub async fn transcribe_audio(&mut self) {
        let audio = Audio::new(&self.client);

        let recording_path = self
            .recording_path
            .clone()
            .expect("Expected a recording path");

        match audio
            .transcribe(
                CreateTranscriptionRequestArgs::default()
                    .file(recording_path)
                    .model("whisper-1")
                    .build()
                    .map_err(|e| {
                        log::error!("Failed to build the CreateTranscriptionRequestArgs: {e:#?}");
                    })
                    .expect("Expected to CreateTranscriptionRequestArgs"),
            )
            .await
        {
            Ok(transcription) => self.transcription = transcription.text,
            Err(e) => log::error!("Failed to transcribe: {e:#?}"),
        };
    }

    pub async fn input(mut self) {
        self.stop();

        let maybe_path = loop {
            {
                log::debug!("Transcription::input() Trying to get a lock on path");
                let guard = self.path.lock().expect("Failed to lock path mutex");
                if let Some(ref path) = *guard {
                    break Some(path.clone());
                }
            }
            sleep(Duration::from_millis(10)).await;
        };

        if let Some(path) = maybe_path {
            self.recording_path = Some(path);
        }

        log::debug!("Transcription.recording_path: {:#?}", self.recording_path);
        self.transcribe_audio().await;

        if let Err(e) = self.sender.send(self.transcription) {
            log::error!("Failed to send the transcription: {e:#?}");
        }
        crate::save::clean_recording_temp_dir();
    }
}

pub fn record_audio(dir: AudioDir, is_recording: Arc<AtomicBool>) -> Result<PathBuf> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| AudioError::AudioRecordingError("No input device available".into()))?;

    let config = device
        .default_input_config()
        .map_err(|e| AudioError::AudioRecordingError(e.to_string()))?;

    let spec = wav_spec_from_config(&config);
    let time = chrono::Local::now().format("%y_%m_%d_%h_%m_%s");
    let recording_path = match &dir {
        AudioDir::TempDir(p) | AudioDir::GameDir(p) => p.join(format!("{}_recording.wav", time)),
    };
    log::info!("recording_path: {recording_path:#?}");

    let writer = hound::WavWriter::create(&recording_path, spec).map_err(AudioError::Hound)?;
    let writer = Arc::new(Mutex::new(Some(writer)));
    let writer_clone = writer.clone();

    let err_fn = move |err| {
        eprintln!("an error occurred on stream: {}", err);
    };

    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| write_input_data::<i8, i8>(data, &writer_clone),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| write_input_data::<i16, i16>(data, &writer_clone),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| write_input_data::<i32, i32>(data, &writer_clone),
            err_fn,
            None,
        ),
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| write_input_data::<f32, f32>(data, &writer_clone),
            err_fn,
            None,
        ),
        sample_format => {
            return Err(AudioError::AudioRecordingError(format!(
                "Unsupported sample format '{sample_format}'"
            ))
            .into());
        }
    };

    let stream = match stream {
        Ok(stream) => stream,
        Err(e) => return Err(AudioError::CpalBuildStream(e).into()),
    };

    stream.play().map_err(AudioError::CpalPlayStream)?;

    // Recording loop
    while is_recording.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }

    // Stop the stream (end recording)
    drop(stream);

    // Finalize the WAV file
    if let Ok(mut guard) = writer.lock() {
        if let Some(writer) = guard.take() {
            writer.finalize().map_err(AudioError::Hound)?;
        }
    }

    // TODO:  I would probably need to change that to stream the audio
    Ok(recording_path)
}

fn sample_format(format: cpal::SampleFormat) -> hound::SampleFormat {
    if format.is_float() {
        hound::SampleFormat::Float
    } else {
        hound::SampleFormat::Int
    }
}

fn wav_spec_from_config(config: &cpal::SupportedStreamConfig) -> hound::WavSpec {
    hound::WavSpec {
        channels: config.channels() as _,
        sample_rate: config.sample_rate().0 as _,
        bits_per_sample: (config.sample_format().sample_size() * 8) as _,
        sample_format: sample_format(config.sample_format()),
    }
}

type WavWriterHandle = Arc<Mutex<Option<hound::WavWriter<BufWriter<fs::File>>>>>;

fn write_input_data<T, U>(input: &[T], writer: &WavWriterHandle)
where
    T: Sample,
    U: Sample + hound::Sample + FromSample<T>,
{
    if let Ok(mut guard) = writer.try_lock() {
        if let Some(writer) = guard.as_mut() {
            for &sample in input.iter() {
                let sample: U = U::from_sample(sample);
                writer.write_sample(sample).ok();
            }
        }
    }
}

pub fn get_sound_asset_path(file_name: &str) -> Option<PathBuf> {
    log::debug!("Assets dir; {:#?}", ASSETS_DIR);
    let sounds_dir = ASSETS_DIR
        .get_dir("sounds")
        .expect("Failed to get sounds directory");

    sounds_dir
        .entries()
        .iter()
        .filter_map(|entry| entry.as_file()) // Only keep files
        .find(|file| (file.path().file_stem().and_then(|stem| stem.to_str()) == Some(file_name)))
        .map(|file| file.path().to_path_buf())
}

pub fn get_all_sounds<'a>() -> Vec<&'a iFile<'a>> {
    let sounds_dir = ASSETS_DIR
        .get_dir("sounds")
        .expect("Failed to get sounds directory");

    fn get_sounds_file<'a>(dir: &'a Dir<'a>) -> Vec<&'a iFile<'a>> {
        let mut sound_files = Vec::new();
        for entry in dir.entries() {
            match entry {
                DirEntry::File(file) => {
                    if file.path().extension().and_then(|ext| ext.to_str()) == Some("mp3") {
                        sound_files.push(file);
                    }
                }
                DirEntry::Dir(subdir) => {
                    sound_files.extend(get_sounds_file(subdir));
                }
            }
        }
        sound_files
    }

    get_sounds_file(sounds_dir)
}

pub fn warm_up_audio() {
    if let Ok((_stream, stream_handle)) = OutputStream::try_default() {
        if let Ok(sink) = Sink::try_new(&stream_handle) {
            // Play 0.1s of silence
            use rodio::source::SineWave;
            sink.append(SineWave::new(0.1).take_duration(std::time::Duration::from_millis(100)));
            sink.detach(); // Don't wait for it
        }
    }
}
