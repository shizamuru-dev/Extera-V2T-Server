use amqprs::{
    channel::{BasicAckArguments, BasicConsumeArguments, BasicRejectArguments},
    consumer::AsyncConsumer,
    BasicProperties, Deliver,
};
use shared::TranscriptionTask;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tracing::{error, info, warn};

struct TranscriptionConsumer {
    channel: amqprs::channel::Channel,
}

impl TranscriptionConsumer {
    pub fn new(channel: amqprs::channel::Channel) -> Self {
        Self { channel }
    }
}

#[async_trait::async_trait]
impl AsyncConsumer for TranscriptionConsumer {
    async fn consume(
        &mut self,
        channel: &amqprs::channel::Channel,
        deliver: Deliver,
        _basic_properties: BasicProperties,
        content: Vec<u8>,
    ) {
        let delivery_tag = deliver.delivery_tag();
        let task_json = match String::from_utf8(content) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to parse payload: {}", e);
                let _ = channel.basic_reject(BasicRejectArguments::new(delivery_tag, false)).await;
                return;
            }
        };

        let task: TranscriptionTask = match serde_json::from_str(&task_json) {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to deserialize task: {}", e);
                let _ = channel.basic_reject(BasicRejectArguments::new(delivery_tag, false)).await;
                return;
            }
        };

        info!("Processing task {}: {:?}", task.task_id, task.file_path);

        let wav_path = format!("/tmp/transcribe/{}.wav", task.task_id);
        let result_json_path = format!("/tmp/transcribe/{}.json", task.task_id);

        // 1. Конвертация аудио через ffmpeg (16kHz, mono) - требование whisper.cpp
        info!("Converting {} to 16kHz WAV...", task.file_path);
        let ffmpeg_status = Command::new("ffmpeg")
            .arg("-y") // перезаписывать если есть
            .arg("-i")
            .arg(&task.file_path)
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("-c:a")
            .arg("pcm_s16le")
            .arg(&wav_path)
            .status()
            .await;

        match ffmpeg_status {
            Ok(status) if status.success() => {
                info!("Converted to WAV successfully.");
                
                // 2. Запуск whisper-cli
                info!("Running whisper.cpp on {}...", wav_path);
                // whisper-cli с флагом -oj сгенерирует файл wav_path + ".json"
                let whisper_status = Command::new("whisper-cli")
                    .arg("-m")
                    .arg("/models/ggml-small.bin") // используем small модель
                    .arg("-l")
                    .arg("auto") // автоопределение языка
                    .arg("-f")
                    .arg(&wav_path)
                    .arg("-oj") // output json
                    .status()
                    .await;

                match whisper_status {
                    Ok(status) if status.success() => {
                        let whisper_output_json = format!("{}.json", wav_path);
                        
                        // Читаем сгенерированный JSON, извлекаем только текст и сохраняем
                        if let Ok(file_content) = fs::read_to_string(&whisper_output_json).await {
                            if let Ok(parsed_json) = serde_json::from_str::<serde_json::Value>(&file_content) {
                                let mut full_text = String::new();
                                if let Some(transcription) = parsed_json.get("transcription").and_then(|t| t.as_array()) {
                                    for item in transcription {
                                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                            full_text.push_str(text);
                                        }
                                    }
                                }
                                
                                let final_result = serde_json::json!({
                                    "text": full_text.trim()
                                });
                                
                                if let Ok(result_string) = serde_json::to_string(&final_result) {
                                    if fs::write(&result_json_path, result_string).await.is_ok() {
                                        info!("Transcription complete. Saved to {}", result_json_path);
                                    } else {
                                        error!("Failed to save final JSON for task {}", task.task_id);
                                    }
                                } else {
                                    error!("Failed to serialize final JSON for task {}", task.task_id);
                                }
                            } else {
                                error!("Failed to parse whisper output JSON for task {}", task.task_id);
                            }
                        } else {
                            error!("Failed to read whisper output JSON for task {}", task.task_id);
                        }
                        
                        let _ = fs::remove_file(&whisper_output_json).await;
                    }
                    _ => error!("Whisper failed for task {}", task.task_id),
                }
            }
            _ => error!("FFmpeg failed to convert {}", task.file_path),
        }

        // Очищаем временные файлы
        let _ = fs::remove_file(&task.file_path).await;
        let _ = fs::remove_file(&wav_path).await;

        // Подтверждаем RabbitMQ
        match channel.basic_ack(BasicAckArguments::new(delivery_tag, false)).await {
            Ok(_) => info!("Acked task {}", task.task_id),
            Err(e) => error!("Failed to ack task {}: {}", task.task_id, e),
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .compact()
        .init();

    info!("Starting transcription worker...");

    let rabbitmq_host = std::env::var("RABBITMQ_HOST").unwrap_or_else(|_| "localhost".to_string());
    let rabbitmq_user = std::env::var("RABBITMQ_USER").unwrap_or_else(|_| "guest".to_string());
    let rabbitmq_pass = std::env::var("RABBITMQ_PASS").unwrap_or_else(|_| "guest".to_string());
    
    // Подключаемся к RabbitMQ (пытаемся до победного)
    let connection = loop {
        match amqprs::connection::Connection::open(
            &amqprs::connection::OpenConnectionArguments::new(
                &rabbitmq_host, 5672, &rabbitmq_user, &rabbitmq_pass,
            ),
        )
        .await
        {
            Ok(c) => break c,
            Err(e) => {
                warn!("Failed to connect to RabbitMQ ({}), retrying in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    let channel = connection
        .open_channel(None)
        .await
        .expect("Failed to open RabbitMQ channel");

    // Создаем очередь если её нет
    channel
        .queue_declare(amqprs::channel::QueueDeclareArguments::new("transcribe.task"))
        .await
        .expect("Failed to declare queue");

    // Создаем обменник (exchange), если его нет
    channel
        .exchange_declare(amqprs::channel::ExchangeDeclareArguments::new(
            "transcribe",
            "direct",
        ))
        .await
        .unwrap_or_default();

    // Биндим
    channel
        .queue_bind(amqprs::channel::QueueBindArguments::new(
            "transcribe.task",
            "transcribe",
            "transcribe.task",
        ))
        .await
        .unwrap_or_default();

    let consumer = TranscriptionConsumer::new(channel.clone());

    // Устанавливаем QoS: prefetch_count = 1
    // Это гарантирует, что RabbitMQ будет выдавать воркеру только 1 задачу за раз.
    // Следующая задача придет только после того, как текущая получит ack.
    channel
        .basic_qos(amqprs::channel::BasicQosArguments::new(0, 1, false))
        .await
        .expect("Failed to set QoS");

    info!("Waiting for messages on queue 'transcribe.task'...");

    channel
        .basic_consume(
            consumer,
            BasicConsumeArguments::new("transcribe.task", "worker_consumer"),
        )
        .await
        .expect("Failed to consume");

    // Блокируем главный поток, чтобы программа не завершилась
    tokio::signal::ctrl_c().await.unwrap();
    info!("Shutting down worker...");
}