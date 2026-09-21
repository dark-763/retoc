use crate::{
    EIoChunkType, FPackageId, UEPath, UEPathBuf, align_usize,
    chunk_id::FIoChunkIdRaw,
    container_header::{EIoContainerHeaderVersion, FIoContainerHeader, StoreEntry},
};
use crate::{EIoStoreTocVersion, FIoChunkHash, FIoChunkId, FIoContainerId, FIoOffsetAndLength, FIoStoreTocCompressedBlockEntry, FIoStoreTocEntryMeta, FIoStoreTocEntryMetaFlags, Toc, ser::*};
use crate::compression::{CompressionMethod, compress};
use anyhow::{Context, Result};
use fs_err as fs;
use rayon::prelude::*;
use std::io::Cursor;
use std::sync::OnceLock;
use std::{
    io::{BufWriter, Seek, Write},
    path::{Path, PathBuf},
};

/// Отдельный пул потоков, и только для сжатия блоков.
///
/// Сжатие вызывается из цикла записи контейнера, а в `to-zen` этот цикл крутится
/// ВНУТРИ rayon-области: конвертация пакетов идёт по глобальному пулу и отдаёт
/// готовое через канал нулевой ёмкости, то есть каждый отправитель ждёт, пока
/// получатель заберёт.
///
/// Если брать потоки для сжатия из того же глобального пула, получается кольцо:
/// поток-получатель, заблокировавшись на параллельном сжатии, начинает воровать
/// задачи конвертации, они упираются в отправку в канал, принять которую некому -
/// и всё встаёт. Наблюдалось именно так: два часа работы, шесть секунд
/// процессорного времени, 60 МБ памяти.
///
/// Отдельный пул разрывает кольцо: его потоки в канал не отправляют никогда.
static COMPRESSION_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

fn compression_pool() -> &'static rayon::ThreadPool {
    COMPRESSION_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("retoc-compress-{index}"))
            .build()
            .expect("failed to build the compression thread pool")
    })
}

pub struct IoStoreWriter {
    #[allow(unused)]
    toc_path: PathBuf,
    toc_stream: BufWriter<fs::File>,
    cas_stream: BufWriter<fs::File>,
    toc: Toc,
    container_header: Option<FIoContainerHeader>,
    compression_method: Option<CompressionMethod>,
}

impl IoStoreWriter {
    pub fn new<P: AsRef<Path>>(toc_path: P, toc_version: EIoStoreTocVersion, container_header_version: Option<EIoContainerHeaderVersion>, mount_point: UEPathBuf) -> Result<Self> {
        let toc_path = toc_path.as_ref().to_path_buf();
        let name = toc_path.file_stem().unwrap().to_string_lossy();
        let toc_stream = BufWriter::new(fs::File::create(&toc_path)?);
        let cas_stream = BufWriter::new(fs::File::create(toc_path.with_extension("ucas"))?);

        let mut toc = Toc::new();
        toc.compression_block_size = 0x10000;
        toc.version = toc_version;
        toc.container_id = FIoContainerId::from_name(&name);
        toc.directory_index.mount_point = mount_point;
        toc.partition_size = u64::MAX;

        // Сжатие включается переменной окружения RETOC_COMPRESSION (Zlib и др.),
        // чтобы поведение по умолчанию осталось прежним.
        let compression_method = std::env::var("RETOC_COMPRESSION")
            .ok()
            .and_then(|v| CompressionMethod::from_str_ignore_case(&v));
        if let Some(method) = compression_method {
            toc.compression_methods = vec![method];
        }

        let container_header = container_header_version.map(|v| FIoContainerHeader::new(v, toc.container_id));

        Ok(Self {
            toc_path,
            toc_stream,
            cas_stream,
            toc,
            container_header,
            compression_method,
        })
    }
    /// Взять метод сжатия из распакованного дампа, если переменная окружения не задана.
    ///
    /// Явное указание пользователя главнее записанного в дампе, поэтому переменная
    /// перекрывает. Вызывать до записи первого чанка: дальше метод уже участвует в
    /// сжатии и в таблице методов контейнера.
    pub fn use_compression_method_if_unset(&mut self, method: Option<CompressionMethod>) {
        if self.compression_method.is_some() {
            return;
        }
        if let Some(method) = method {
            self.compression_method = Some(method);
            self.toc.compression_methods = vec![method];
        }
    }
    pub fn compression_method(&self) -> Option<CompressionMethod> {
        self.compression_method
    }
    /// Перенять локализацию и редиректы из заголовка, записанного в дампе.
    ///
    /// `pack-raw` наполняет новый заголовок только записями пакетов, а разделы
    /// локализации и редиректов не восстанавливал ничем - они пропадали молча.
    /// Возвращает, сколько записей перенесено, чтобы вызывающий мог это показать.
    pub fn adopt_localization_and_redirects(&mut self, recorded: &FIoContainerHeader) -> (usize, usize) {
        match self.container_header.as_mut() {
            Some(header) => {
                header.adopt_localization_and_redirects(recorded);
                header.localization_and_redirect_counts()
            }
            None => (0, 0),
        }
    }
    pub fn write_chunk_raw(&mut self, chunk_id_raw: FIoChunkIdRaw, path: Option<&UEPath>, data: &[u8]) -> Result<()> {
        self.write_chunk(FIoChunkId::from_raw(chunk_id_raw, self.toc.version), path, data)
    }
    pub fn write_chunk(&mut self, chunk_id: FIoChunkId, path: Option<&UEPath>, data: &[u8]) -> Result<()> {
        if let Some(path) = path {
            let index = &mut self.toc.directory_index;
            let relative_path = path.strip_prefix(&index.mount_point).with_context(|| format!("mount point {} does not contain path {path}", index.mount_point))?;
            index.add_file(relative_path, self.toc.chunks.len() as u32);
        }

        let mut offset = self.cas_stream.stream_position()?;

        let start_block = self.toc.compression_blocks.len();

        // Хеш считается по несжатым данным - так же, как в оригинале. Блоки идут
        // подряд и вместе составляют `data`, поэтому обновление по блокам и одно
        // обновление целиком дают один и тот же хеш.
        let mut hasher = blake3::Hasher::new();
        hasher.update(data);

        let block_size = self.toc.compression_block_size as usize;
        let method = self.compression_method;

        // Блоки сжимаются параллельно, а пишутся строго по порядку. Раньше и то и
        // другое шло одним последовательным циклом: 18 ГБ порциями по 64 КБ на одном
        // ядре из шестнадцати, и это была основная часть времени сборки контейнера.
        //
        // `None` означает "писать блок как есть": блок сжимается, только если это
        // даёт выигрыш, неужавшиеся пишутся с методом 0 - оригинальный кукер
        // поступает так же.
        let compressed: Vec<Option<Vec<u8>>> = match method {
            Some(method) => compression_pool().install(|| {
                data.par_chunks(block_size)
                    .map(|block| {
                        let mut buffer: Vec<u8> = Vec::new();
                        if compress(method, block, Cursor::new(&mut buffer)).is_ok() && buffer.len() < block.len() {
                            Some(buffer)
                        } else {
                            None
                        }
                    })
                    .collect()
            }),
            None => Vec::new(),
        };

        for (index, block) in data.chunks(block_size).enumerate() {
            let squeezed = compressed.get(index).and_then(|x| x.as_deref());
            let compression_method_index = if squeezed.is_some() { 1u8 } else { 0u8 };
            let payload: &[u8] = squeezed.unwrap_or(block);

            self.cas_stream.write_all(payload)?;
            let compressed_size = payload.len() as u32;

            self.toc.compression_blocks.push(FIoStoreTocCompressedBlockEntry::new(offset, compressed_size, block.len() as u32, compression_method_index));
            offset += compressed_size as u64;
        }
        let hash = hasher.finalize();
        let meta = FIoStoreTocEntryMeta {
            chunk_hash: FIoChunkHash::from_blake3(hash.as_bytes()),
            flags: FIoStoreTocEntryMetaFlags::empty(),
        };

        let offset_and_length = FIoOffsetAndLength::new(start_block as u64 * self.toc.compression_block_size as u64, data.len() as u64);

        self.toc.chunks.push(chunk_id.with_version(self.toc.version));
        self.toc.chunk_offset_lengths.push(offset_and_length);
        self.toc.chunk_metas.push(meta);

        Ok(())
    }

    pub fn write_package_chunk(&mut self, chunk_id: FIoChunkId, path: Option<&UEPath>, data: &[u8], store_entry: &StoreEntry) -> Result<()> {
        let container_header = self.container_header.as_mut().expect("FIoContainerHeader is required to write package chunks");
        container_header.add_package(FPackageId(chunk_id.get_chunk_id()), store_entry.clone());
        self.write_chunk(chunk_id, path, data)
    }
    pub fn add_localized_package(&mut self, package_culture: &str, source_package_name: &str, localized_package_id: FPackageId) -> Result<()> {
        let container_header = self.container_header.as_mut().expect("FIoContainerHeader is required to add localized packages");
        container_header.add_localized_package(package_culture, source_package_name, localized_package_id)
    }
    pub fn add_package_redirect(&mut self, source_package_name: &str, redirect_package_id: FPackageId) -> Result<()> {
        let container_header = self.container_header.as_mut().expect("FIoContainerHeader is required to add package redirects");
        container_header.add_package_redirect(source_package_name, redirect_package_id)
    }
    pub fn container_version(&self) -> EIoStoreTocVersion {
        self.toc.version
    }
    pub fn container_header_version(&self) -> EIoContainerHeaderVersion {
        self.container_header.as_ref().unwrap().version
    }
    pub fn finalize(mut self) -> Result<()> {
        if let Some(container_header) = &self.container_header {
            let mut chunk_buffer = vec![];
            container_header.serialize(&mut Cursor::new(&mut chunk_buffer))?;
            // container header is always aligned for AES for some reason
            chunk_buffer.resize(align_usize(chunk_buffer.len(), 16), 0);

            let chunk_id = FIoChunkId::create(container_header.container_id.0, 0, EIoChunkType::ContainerHeader);
            self.write_chunk(chunk_id, None, &chunk_buffer)?;
        }
        self.toc_stream.ser(&self.toc)?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fs_err as fs;

    #[test]
    fn test_write_container() -> Result<()> {
        // Во временный каталог, а не в "out" рядом с репозиторием: иначе после
        // каждого `cargo test` в рабочем дереве появляются неотслеживаемые файлы.
        let out = std::env::temp_dir().join("retoc-test-iostore-writer");
        fs::create_dir_all(&out)?;
        let mut writer = IoStoreWriter::new(out.join("new.utoc"), EIoStoreTocVersion::PerfectHashWithOverflow, Some(EIoContainerHeaderVersion::OptionalSegmentPackages), "../../..".into())?;

        let data = fs::read("tests/UE5.3/ScriptObjects.bin")?;
        writer.write_chunk_raw(FIoChunkIdRaw { id: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5] }, Some(UEPath::new("../../../asdf/asdf/dasf/script_objects.bin")), &data)?;
        writer.finalize()?;
        Ok(())
    }
}
