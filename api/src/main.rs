use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use shared::{TranscriptionResponse, TranscriptionTask};
use serde::Deserialize;
use sha2::{Sha256, Digest};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use uuid::Uuid;

// ============================================================================
// ТИПЫ ОШИБОК
// ============================================================================

#[derive(Debug)]
enum TranscribeError {
    FileOperation(String),
    RabbitMQ(String),
    Validation(String),
}

impl IntoResponse for TranscribeError {
    fn into_response(self) -> axum::response::Response {
        let (status, error_message) = match self {
            TranscribeError::Validation(msg) => (StatusCode::BAD_REQUEST, msg),
            TranscribeError::FileOperation(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            TranscribeError::RabbitMQ(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
        };

        let body = Json(serde_json::json!({
            "error": error_message
        }));

        (status, body).into_response()
    }
}

// ============================================================================
// СТРУКТУРЫ ДЛЯ RABITMQ
// ============================================================================

// Убрали TranscriptionTask и TranscriptionResponse, они теперь в shared

// ============================================================================
// RABITMQ ПУБЛИШЕР
// ============================================================================

#[derive(Clone)]
struct RabbitMQPublisher {
    channel: amqprs::channel::Channel,
}

impl RabbitMQPublisher {
    fn new(channel: amqprs::channel::Channel) -> Self {
        Self { channel }
    }

    /// Отправляет задачу в RabbitMQ очередь
    async fn publish_task(&self, task: &TranscriptionTask) -> Result<(), TranscribeError> {
        info!(
            task_id = %task.task_id,
            file_path = %task.file_path,
            "Publishing task to RabbitMQ"
        );

        // Преобразуем задачу в JSON
        let json_payload = serde_json::to_vec(task).map_err(|e| {
            TranscribeError::RabbitMQ(format!("JSON serialization failed: {}", e))
        })?;

        let mut props = amqprs::BasicProperties::default();
        props.with_delivery_mode(2);

        // Объявляем обменник перед отправкой (на случай если воркер еще не поднялся)
        self.channel
            .exchange_declare(amqprs::channel::ExchangeDeclareArguments::new(
                "transcribe",
                "direct",
            ))
            .await
            .map_err(|e| TranscribeError::RabbitMQ(format!("Failed to declare exchange: {}", e)))?;

        let payload_size = json_payload.len();
        self.channel
            .basic_publish(
                props,
                json_payload,
                amqprs::channel::BasicPublishArguments::new("transcribe", "transcribe.task"),
            )
            .await
            .map_err(|e| TranscribeError::RabbitMQ(format!("Publish failed: {}", e)))?;

        info!(
            task_id = %task.task_id,
            payload_size = payload_size,
            "Task published successfully"
        );

        Ok(())
    }
}

// ============================================================================
// ХРАНИЛИЩЕ ФАЙЛОВ
// ============================================================================

#[derive(Clone)]
struct FileStorage {
    tmp_dir: String,
    max_file_size: u64, // в байтах
}

impl FileStorage {
    fn new(tmp_dir: String, max_file_size_mb: u64) -> Self {
        Self {
            tmp_dir,
            max_file_size: max_file_size_mb * 1024 * 1024,
        }
    }

    /// Проверяет и создаёт временную директорию
    async fn ensure_tmp_dir(&self) -> Result<(), TranscribeError> {
        tokio::fs::create_dir_all(&self.tmp_dir)
            .await
            .map_err(|e| {
                TranscribeError::FileOperation(format!("Cannot create tmp dir: {}", e))
            })?;
        Ok(())
    }

    /// Генерирует безопасный путь до файла
    fn generate_file_path(&self, original_filename: &str) -> Result<String, TranscribeError> {
        // Валидация имени файла
        if original_filename.is_empty() || original_filename.len() > 255 {
            return Err(TranscribeError::Validation(
                "Invalid filename length".to_string(),
            ));
        }

        // Убираем опасные символы
        let safe_filename = original_filename
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
            .collect::<String>();

        if safe_filename.is_empty() {
            return Err(TranscribeError::Validation(
                "Filename contains no valid characters".to_string(),
            ));
        }

        // Генерируем уникальное имя: UUID + исходное расширение
        let ext = Path::new(&safe_filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("ogg");
        let unique_filename = format!("{}_{}.{}", Uuid::new_v4(), safe_filename, ext);

        Ok(format!("{}/{}", self.tmp_dir, unique_filename))
    }

    /// Сохраняет файл потоком (streaming) и одновременно вычисляет SHA256 хеш
    async fn save_file_streaming_with_hash(
        &self,
        file_path: &str,
        mut multipart_field: axum::extract::multipart::Field<'_>,
    ) -> Result<(u64, String), TranscribeError> {
        let file = tokio::fs::File::create(file_path).await.map_err(|e| {
            TranscribeError::FileOperation(format!("Cannot create file: {}", e))
        })?;
        let mut file = tokio::io::BufWriter::new(file);

        let mut hasher = Sha256::new();
        let mut total_bytes = 0u64;

        // Потоковое чтение, запись чанков и вычисление хеша одновременно
        while let Some(chunk) = multipart_field.chunk().await.map_err(|e| {
            TranscribeError::FileOperation(format!("Failed to read chunk: {}", e))
        })? {
            let chunk_size = chunk.len() as u64;

            // Проверка максимального размера файла
            if total_bytes + chunk_size > self.max_file_size {
                // Удаляем недописанный файл
                let _ = tokio::fs::remove_file(file_path).await;
                return Err(TranscribeError::Validation(format!(
                    "File size exceeds limit of {} MB",
                    self.max_file_size / (1024 * 1024)
                )));
            }

            // Обновляем хеш
            hasher.update(&chunk);

            // Записываем в файл
            file.write_all(&chunk).await.map_err(|e| {
                TranscribeError::FileOperation(format!("Failed to write chunk: {}", e))
            })?;

            total_bytes += chunk_size;
        }

        // Сбрасываем буфер
        file.flush().await.map_err(|e| {
            TranscribeError::FileOperation(format!("Failed to flush buffer: {}", e))
        })?;

        // Получаем оригинальный файл и синхронизируем на диск
        let file = file.into_inner();
        file.sync_all().await.map_err(|e| {
            TranscribeError::FileOperation(format!("Failed to sync file: {}", e))
        })?;

        // Вычисляем финальный хеш
        let hash_bytes = hasher.finalize();
        let file_hash = hex::encode(hash_bytes);

        info!(
            file_path = %file_path,
            total_bytes = total_bytes,
            file_hash = %file_hash,
            "File saved successfully with hash"
        );

        Ok((total_bytes, file_hash))
    }

    /// Удаляет файл (для rollback при ошибке)
    async fn delete_file(&self, file_path: &str) -> Result<(), TranscribeError> {
        tokio::fs::remove_file(file_path).await.map_err(|e| {
            warn!(file_path = %file_path, error = %e, "Failed to delete file");
            TranscribeError::FileOperation(format!("Cannot delete file: {}", e))
        })
    }
}

// ============================================================================
// ПРИЛОЖЕНИЕ
// ============================================================================

#[derive(Clone)]
struct AppState {
    storage: FileStorage,
    rabbitmq: RabbitMQPublisher,
}

// ============================================================================
// ОБРАБОТЧИКИ
// ============================================================================

#[derive(Debug, Deserialize)]
struct TranscribeQuery {
    chat_id: String,
    message_id: Option<String>,
}

/// Обработчик для загрузки файла
async fn handle_transcribe(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(query_params): axum::extract::Query<TranscribeQuery>,
    mut multipart: Multipart,
) -> Result<Json<TranscriptionResponse>, TranscribeError> {
    // Используем хранилище и RabbitMQ из состояния
    let storage = &state.storage;
    let rabbitmq = &state.rabbitmq;

    // Создаём временную директорию
    storage.ensure_tmp_dir().await?;

    // Обрабатываем multipart форму
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        TranscribeError::FileOperation(format!("Multipart parsing failed: {}", e))
    })? {
        let field_name = field.name().unwrap_or("file").to_string();

        // Обрабатываем только поле "file"
        if field_name != "file" {
            warn!(field_name = %field_name, "Skipping non-file field");
            continue;
        }

        let original_filename = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "audio.ogg".to_string());

        // Проверяем расширение файла
        let ext = Path::new(&original_filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        if !matches!(ext, "ogg" | "mp3" | "wav" | "webm" | "m4a") {
            return Err(TranscribeError::Validation(
                "Unsupported audio format. Allowed: ogg, mp3, wav, webm, m4a".to_string(),
            ));
        }

        // Генерируем безопасный путь
        let file_path = storage.generate_file_path(&original_filename)?;

        // Сохраняем файл потоком и сразу вычисляем хеш
        return match storage.save_file_streaming_with_hash(&file_path, field).await {
            Ok((file_size, file_hash)) => {
                info!(
                    file_path = %file_path,
                    file_size = file_size,
                    file_hash = %file_hash,
                    "File saved and hashed"
                );

                // Проверяем, есть ли уже результат с этим хешем
                let cached_result_path = format!("{}/cache_{}.json", storage.tmp_dir, file_hash);
                if tokio::fs::metadata(&cached_result_path).await.is_ok() {
                    // Валидируем кешированный файл
                    match tokio::fs::read_to_string(&cached_result_path).await {
                        Ok(content) if serde_json::from_str::<serde_json::Value>(&content).is_ok() => {
                            info!(file_hash = %file_hash, "Cache hit! Returning cached result");
                            
                            // Удаляем загруженный файл, он нам не нужен
                            let _ = storage.delete_file(&file_path).await;
                            
                            // Копируем кешированный результат под новый task_id
                            let task_id = Uuid::new_v4().to_string();
                            let result_path = format!("{}/{}.json", storage.tmp_dir, task_id);
                            
                            match tokio::fs::copy(&cached_result_path, &result_path).await {
                                Ok(_) => {
                                    return Ok(Json(TranscriptionResponse {
                                        task_id,
                                        status: "completed".to_string(),
                                        file_path: result_path,
                                    }));
                                }
                                Err(e) => {
                                    warn!(error = %e, "Failed to copy cached result, will process normally");
                                }
                            }
                        }
                        _ => {
                            warn!(file_hash = %file_hash, "Corrupted cache file detected, removing and processing normally");
                            let _ = tokio::fs::remove_file(&cached_result_path).await;
                        }
                    }
                }

                // Создаём задачу для воркера
                let task = TranscriptionTask {
                    task_id: Uuid::new_v4().to_string(),
                    file_path: file_path.clone(),
                    chat_id: query_params.chat_id.clone(),
                    message_id: query_params.message_id.clone(),
                    filename: original_filename.clone(),
                    created_at: chrono::Utc::now().timestamp(),
                    file_hash,
                };

                // Отправляем в RabbitMQ
                match rabbitmq.publish_task(&task).await {
                    Ok(_) => {
                        Ok(Json(TranscriptionResponse {
                            task_id: task.task_id,
                            status: "queued".to_string(),
                            file_path,
                        }))
                    }
                    Err(e) => {
                        // Если не смогли отправить в RabbitMQ, удаляем файл
                        let _ = storage.delete_file(&file_path).await;
                        Err(e)
                    }
                }
            }
            Err(e) => {
                // При ошибке сохранения удаляем файл если он создан
                let _ = storage.delete_file(&file_path).await;
                Err(e)
            }
        }
    }

    Err(TranscribeError::Validation(
        "No file uploaded".to_string(),
    ))
}

/// Healthcheck эндпоинт
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "transcribe-api"
    }))
}

/// Обработчик для получения результата транскрибации
async fn get_result(
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::response::Response {
    if !task_id.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return (StatusCode::BAD_REQUEST, "Invalid task ID").into_response();
    }

    let result_path = format!("{}/{}.json", state.storage.tmp_dir, task_id);

    match tokio::fs::read_to_string(&result_path).await {
        Ok(json_content) => {
            match serde_json::from_str::<serde_json::Value>(&json_content) {
                Ok(json) => Json(json).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Invalid JSON result").into_response(),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Файла с результатом еще нет — задача в очереди или обрабатывается воркером
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "task_id": task_id,
                    "status": "processing"
                })),
            )
                .into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Error reading result").into_response(),
    }
}

// ============================================================================
// MAIN
// ============================================================================

/// Фоновая задача для удаления старых файлов результатов и кеша
async fn cleanup_task(dir: String, result_max_age_secs: u64, cache_max_age_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 10)); // Каждые 10 минут
    loop {
        interval.tick().await;
        
        let mut deleted_results = 0;
        let mut deleted_cache = 0;
        
        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Ok(metadata) = entry.metadata().await {
                    if let Ok(modified) = metadata.modified() {
                        if let Ok(elapsed) = modified.elapsed() {
                            let path = entry.path();
                            let filename = path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("");
                            
                            // Кеш-файлы (cache_*.json) удаляются через cache_max_age_secs
                            if filename.starts_with("cache_") && filename.ends_with(".json") {
                                if elapsed.as_secs() > cache_max_age_secs {
                                    if tokio::fs::remove_file(&path).await.is_ok() {
                                        deleted_cache += 1;
                                    }
                                }
                            }
                            // Обычные результаты (UUID.json) удаляются через result_max_age_secs
                            else if filename.ends_with(".json") {
                                if elapsed.as_secs() > result_max_age_secs {
                                    if tokio::fs::remove_file(&path).await.is_ok() {
                                        deleted_results += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        
        if deleted_results > 0 || deleted_cache > 0 {
            info!(
                deleted_results = deleted_results,
                deleted_cache = deleted_cache,
                "Cleanup completed"
            );
        }
    }
}

#[tokio::main]
async fn main() {
    // Инициализируем логирование
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .compact()
        .init();

    info!("Starting transcribe service...");

    // Инициализация RabbitMQ
    let rabbitmq_host = std::env::var("RABBITMQ_HOST").unwrap_or_else(|_| "localhost".to_string());
    let rabbitmq_user = std::env::var("RABBITMQ_USER").unwrap_or_else(|_| "guest".to_string());
    let rabbitmq_pass = std::env::var("RABBITMQ_PASS").unwrap_or_else(|_| "guest".to_string());
    let connection = amqprs::connection::Connection::open(
        &amqprs::connection::OpenConnectionArguments::new(
            &rabbitmq_host, 5672, &rabbitmq_user, &rabbitmq_pass,
        ),
    )
    .await
    .expect("Failed to connect to RabbitMQ");

    let channel = connection
        .open_channel(None)
        .await
        .expect("Failed to open RabbitMQ channel");

    let state = AppState {
        storage: FileStorage::new("/tmp/transcribe".to_string(), 500),
        rabbitmq: RabbitMQPublisher::new(channel),
    };

    // Запускаем фоновую задачу по очистке старых файлов
    // Результаты (UUID.json) удаляются через 1 час (3600 сек)
    // Кеш-файлы (cache_*.json) удаляются через 7 дней (604800 сек)
    tokio::spawn(cleanup_task("/tmp/transcribe".to_string(), 3600, 604800));

    // Создаём роутер
    let app = Router::new()
        .route("/health", get(health))
        .route("/result/:task_id", get(get_result))
        .route(
            "/transcribe",
            post(handle_transcribe)
                // Лимит на размер тела запроса (500 MB)
                .layer(DefaultBodyLimit::max(500 * 1024 * 1024)),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // Запускаем сервер
    let listen_host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let bind_addr = format!("{}:3000", listen_host);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|_| panic!("Failed to bind to {}", bind_addr));

    info!("Server running on http://{}:3000", listen_host);
    info!("Transcribe endpoint: POST http://{}:3000/transcribe", listen_host);

    axum::serve(listener, app).await.expect("Server error");
}
