//! MDX dictionary reader for high-level dictionary lookups.
//!
//! This module provides the main API for reading MDX (MDict) dictionary files.
//! It handles:
//! - Dictionary content lookup and retrieval
//! - Optional resource file (MDD) loading
//! - Full-text search support
//! - HTML link rewriting
//! - Compact stylesheet decompression
//!
//! # Examples
//!
//! ```no_run
//! use mdx::mdx_reader::MdxReader;
//! use url::Url;
//!
//! # fn main() -> mdx::Result<()> {
//! // Open an MDX dictionary file
//! let url = Url::parse("file:///path/to/dictionary.mdx")?;
//! let mut reader = MdxReader::from_url(&url, "device_id")?;
//!
//! // Look up a word
//! let key_index = reader.lookup("hello")?;
//! let definition = reader.get_html(&key_index)?;
//! println!("Definition: {}", definition);
//! # Ok(())
//! # }
//! ```
//!
//! # Features
//!
//! - **Dictionary Lookup**: Fast key-based lookup with support for partial matches
//! - **Content Retrieval**: Get content in raw bytes, plain text, or HTML format
//! - **Full-Text Search**: Search through dictionary content if an index is available
//! - **Resource Loading**: Automatically load associated MDD files for images and media
//! - **HTML Rewriting**: Convert internal links to MDX protocol format

use std::collections::LinkedList;
use std::fs::File;
use std::io::BufReader;

use log::*;
use mime_guess::MimeGuess;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::{Index, TantivyDocument};
use url::Url;

use crate::utils::io_utils::{load_string_from_file_with_ext, open_file_url_as_reader};
use crate::storage::key_block::{EntryNo, KeyIndex};
use crate::utils::url_utils::{self, with_extension};
use super::mdd_reader::MddReader;
use crate::storage::meta_unit::ContentType;
use crate::utils::html_escape_mdx_text;
use super::zdb_reader::ZdbReader;
use crate::storage::zip_directory::ZipDirectory;
use crate::{Result, ZdbError};
const MDICT_INDEX_EXT: &str = "idx";
const MDICT_MDD_EXT: &str = "mdd";
const MDICT_KEY_EXT: &str = "key";

/// High-level MDX dictionary reader.
///
/// This struct provides the main interface for reading MDict (MDX) dictionary files.
/// It manages the content database, optional resource database, and full-text search index.
pub struct MdxReader {
    /// The main content database reader
    pub content_db: ZdbReader<BufReader<File>>,
    /// Optional associated resource (MDD) file reader
    pub data_db: Option<MddReader>,
    /// Optional full-text search index
    pub fts_index: Option<Index>,
    /// Name of the dictionary
    pub db_name: String,
    /// URL to the MDX file
    pub mdx_url: Url,
    /// Compact stylesheet for decompacting content
    compact_stylesheet: Vec<(String, String)>,
}

impl MdxReader {

    /// Opens an MDX dictionary file from a URL.
    ///
    /// This method attempts to load:
    /// 1. The main MDX content file
    /// 2. Associated MDD resource file (if exists)
    /// 3. Full-text search index (if exists)
    ///
    /// Missing optional resources do not cause failures; the reader will still function
    /// with those features unavailable.
    ///
    /// # Arguments
    ///
    /// * `mdx_url` - URL to the MDX file (typically `file:///path/to/file.mdx`)
    /// * `device_id` - Device identifier for license verification
    ///
    /// # Returns
    ///
    /// Returns an initialized MdxReader on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the main MDX file cannot be opened or parsed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use mdx::mdx_reader::MdxReader;
    /// use url::Url;
    ///
    /// let url = Url::parse("file:///dict/Oxford.mdx")?;
    /// let reader = MdxReader::from_url(&url, "my_device")?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_url(mdx_url: &Url, device_id: &str) -> Result<Self> {
        let mdx_url = mdx_url.clone();
        let reader = open_file_url_as_reader(&mdx_url)?;
        let license_data = load_string_from_file_with_ext(&mdx_url, MDICT_KEY_EXT)?;
        let content_db = ZdbReader::<BufReader<File>>::from_reader(reader, device_id, &license_data)?;
        
        // Try to initialize data_db, but allow it to fail
        let data_db = match MddReader::from_url(&with_extension(&mdx_url, MDICT_MDD_EXT)?, device_id) {
            Ok(db) => Some(db),
            Err(e) => {
                warn!("Failed to load MDD data database: {}. Data resources will not be available.", e);
                None
            }
        };
        
        let db_name= url_utils::get_decoded_file_stem(&mdx_url)?;
        // Compact 样式表只影响渲染；即便解析失败也只能用空表兜底，绝不能像
        // 旧版那样用 `?` 把错误冒泡、导致整本词典打不开（普通 CSS 样式表的
        // 词典曾因此无法加载）。data_db / fts_index 同样允许失败。
        let compact_stylesheet = match Self::load_compact_stylesheet(&content_db.meta.db_info.style_sheet) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to load compact stylesheet, continue without it: {}", e);
                Vec::new()
            }
        };
        
        // Try to initialize FTS index, but allow it to fail
        let fts_index = match Self::load_fts_index(&with_extension(&mdx_url, MDICT_INDEX_EXT)?) {
            Ok(index) => Some(index),
            Err(e) => {
                info!("Failed to load FTS index: {}. Full-text search will not be available.", e);
                None
            }
        };
        let mdx_reader = Self { content_db, data_db, fts_index, db_name, mdx_url, compact_stylesheet };
        Ok(mdx_reader)
    }


    /// Gets multiple key indexes starting from a specific entry number.
    ///
    /// # Arguments
    ///
    /// * `start_entry_no` - Starting entry number
    /// * `max_count` - Maximum number of entries to retrieve
    ///
    /// # Returns
    ///
    /// Returns a LinkedList of KeyIndex entries.
    pub fn get_indexes(&mut self, start_entry_no: EntryNo, max_count: u64) -> Result<LinkedList<KeyIndex>> {
        self.content_db.get_indexes(start_entry_no, max_count)
    }

    /// Gets a single key index by entry number.
    ///
    /// # Arguments
    ///
    /// * `entry_no` - Entry number to retrieve
    ///
    /// # Returns
    ///
    /// Returns the KeyIndex for the given entry number.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry number is invalid.
    pub fn get_index(&mut self, entry_no: EntryNo) -> Result<KeyIndex> {
        self.content_db.get_index(entry_no)
    }

    /// Gets raw (unprocessed) content bytes for a dictionary entry.
    ///
    /// # Arguments
    ///
    /// * `key_index` - The key index of the entry
    ///
    /// # Returns
    ///
    /// Returns the raw content bytes.
    pub fn get_raw(&mut self, key_index: &KeyIndex) -> Result<Vec<u8>> {
        self.content_db.get_data(key_index, false)
    } 

    /// Gets content as a string for a dictionary entry.
    ///
    /// This method can optionally decompact content that uses the compact stylesheet format.
    ///
    /// # Arguments
    ///
    /// * `key_index` - The key index of the entry
    /// * `decompact` - Whether to decompact the content using the stylesheet
    ///
    /// # Returns
    ///
    /// Returns the content as a UTF-8 string.
    pub fn get_string(&mut self, key_index: &KeyIndex, decompact: bool) -> Result<String> {
        if decompact && !self.compact_stylesheet.is_empty() {
			let compacted_content = self.content_db.get_string(key_index, true)?;
            Self::reformat(&compacted_content, &self.compact_stylesheet)
        } else {
            self.content_db.get_string(key_index, true)
        }
    }

    /// Gets content as HTML for a dictionary entry.
    ///
    /// This method automatically converts text content to HTML-escaped format
    /// and retrieves HTML content as-is.
    ///
    /// # Arguments
    ///
    /// * `key_index` - The key index of the entry
    ///
    /// # Returns
    ///
    /// Returns the content as HTML.
    ///
    /// # TODO
    ///
    /// Need to rebuild links in HTML to use mdx schema (mdx://)
    pub fn get_html(&mut self, key_index: &KeyIndex) -> Result<String> {
        //TODO Need to rebuild links in html to use mdx schema (mdx://)
        let content_type = self.content_db.meta.db_info.content_type.clone();
        match content_type {
            ContentType::Text => {
                let mut buffer = String::with_capacity(1024);
                html_escape_mdx_text(&self.get_string(key_index, true)?, &mut buffer);
                Ok(buffer)
            }
            ContentType::Html => {
                self.get_string(key_index, true)
            }
            _ => Err(ZdbError::invalid_data_format("Db content type is not supported")),
        }
    }

	/// Expand compacted content using stylesheet tokens surrounded by backticks.
	/// Tokens are specified as `number` where number is 0..255 and map to
	/// `compact_stylesheet[token] = (prefix, suffix)`.
    /// 
    /// Expand compacted content using stylesheet tokens surrounded by backticks.
    /// 与 Python 版 `MDX._substitute_stylesheet` 逐字对齐：
    ///   - `` `N`body` `` -> prefix + body + suffix；
    ///   - body 的闭合反引号（其后不是新 token 数字）被消费，不残留到正文；
    ///   - 未知 token 原样保留，绝不 panic（旧实现对越界 token 直接索引会崩）。
    pub fn reformat(compacted_source: &str, compact_style: &[(String, String)]) -> Result<String> {
        let chars: Vec<char> = compacted_source.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(n + 1024);
        let mut i = 0usize;
        while i < n {
            if chars[i] != '`' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            // 读 `N` 里的数字 token
            let mut j = i + 1;
            while j < n && chars[j].is_ascii_digit() {
                j += 1;
            }
            if !(j > i + 1 && j < n && chars[j] == '`') {
                // 不是合法 token 起点，反引号当普通字符
                out.push('`');
                i += 1;
                continue;
            }
            let token_digits: String = chars[i + 1..j].iter().collect();
            let token: usize = token_digits.parse().unwrap_or(usize::MAX);
            i = j + 1; // 跳过 `N` 的前半个反引号与数字

            // body 取到下一个反引号
            let body_start = i;
            while i < n && chars[i] != '`' {
                i += 1;
            }
            let mut body: String = chars[body_start..i].iter().collect();

            // 若该反引号后不是数字，说明它是本段的闭合符，消费掉。
            let mut closed = false;
            if i < n {
                let after = i + 1;
                if after >= n || !chars[after].is_ascii_digit() {
                    i += 1;
                    closed = true;
                }
            }

            if token >= compact_style.len() {
                // 未知 / 越界 token：原样保留整段，不 panic
                out.push('`');
                out.push_str(&token_digits);
                out.push('`');
                out.push_str(&body);
                if closed {
                    out.push('`');
                }
                continue;
            }

            // 合法 token：prefix + body + suffix；body 以 \n 结尾时把换行留在样式外
            if body.ends_with('\n') {
                while body.ends_with('\n') || body.ends_with('\r') {
                    body.pop();
                }
                out.push_str(&compact_style[token].0);
                out.push_str(&body);
                out.push_str(&compact_style[token].1);
                out.push_str("\r\n");
            } else {
                out.push_str(&compact_style[token].0);
                out.push_str(&body);
                out.push_str(&compact_style[token].1);
            }
        }
        Ok(out)
    }

    pub fn get_data(&mut self, file_path: &str) -> Result<Option<(Vec<u8>, String)>> {
        // Handle data database lookup
        if self.data_db.is_some() {
            let buffer = self.data_db.as_mut().unwrap().get_data_by_path(file_path, true)?;
            if let Some(buffer) = buffer {
                let mime_type = MimeGuess::from_path(file_path).first_or_octet_stream().to_string();
                    return Ok(Some((buffer, mime_type)));
            }
        }
        Ok(None)
    }

    pub fn get_entry_count(&self) -> u64 {
        self.content_db.get_entry_count()
    }

    pub fn find_index(&mut self, key: &str, prefix_match: bool, partial_match: bool, best_match: bool) -> Result<Option<KeyIndex>> {
        self.content_db.find_first_match(key, prefix_match, partial_match, best_match)
    }

    pub fn get_similar_indexes(&mut self, key_index: &KeyIndex, start_with: bool, max_count: u64) -> Result<LinkedList<KeyIndex>> {
        self.content_db.get_similar_indexes(key_index, start_with, max_count)
    }

    /// Load compact stylesheet triples: token, prefix, suffix (newline-separated).
    ///
    /// MDX header 的 `StyleSheet` 有两种形态，必须区分对待（对齐 Python 版
    /// `MDX._normalize_stylesheet`）：
    ///   1. compact 样式表：每三行一组 `token / prefix / suffix`，token 是 0..255
    ///      的数字，prefix/suffix 可含 HTML 实体；
    ///   2. 普通 CSS 样式表（供前端使用）：行首不是纯数字 token。
    ///
    /// 旧实现把「非数字 token」当成致命错误并向上抛，导致带普通 CSS 样式表的
    /// 词典整本加载失败。这里改为容错：任何一个三元组解析不出来就跳过它，
    /// 若最终没有任何合法 token，返回空 vec 表示"不是 compact 样式表"，
    /// 调用方据此跳过 compact 还原——与 Python 版行为一致。
    ///
    /// 永远不返回 Err：样式表只是渲染增强，绝不能让整本词典打不开。
    pub fn load_compact_stylesheet(style_sheet: &str) -> Result<Vec<(String, String)>> {
        if style_sheet.is_empty() {
            return Ok(Vec::new());
        }
        let mut compact_stylesheet = vec![(String::new(), String::new()); 256];
        // lines() 同时兼容 \n 与 \r\n；strip 掉每行首尾空白（含残留的 \r）。
        let lines: Vec<&str> = style_sheet.lines().collect();
        let mut has_stylesheet = false;
        let mut i = 0usize;
        while i + 2 < lines.len() {
            let token_line = lines[i].trim();
            // 非数字 token：这是普通 CSS 样式表或注释行，跳过这一行继续找，
            // 而不是把整本词典判为损坏。
            let token: u32 = match token_line.parse() {
                Ok(t) => t,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };
            if token > 255 {
                i += 3;
                continue;
            }
            // 三元组不满三行：尾部残留，直接结束。
            if i + 2 >= lines.len() {
                break;
            }
            let prefix_raw = lines[i + 1].trim();
            let suffix_raw = lines[i + 2].trim();
            // HTML 实体解码失败时退回原值，同样不中断解析。
            let prefix = htmlescape::decode_html(prefix_raw)
                .unwrap_or_else(|_| prefix_raw.to_string());
            let suffix = htmlescape::decode_html(suffix_raw)
                .unwrap_or_else(|_| suffix_raw.to_string());
            compact_stylesheet[token as usize] = (prefix, suffix);
            has_stylesheet = true;
            i += 3;
        }
        if !has_stylesheet {
            compact_stylesheet.clear();
        }
        Ok(compact_stylesheet)
    }

    /// Load FTS index from .idx file or directory
    fn load_fts_index(idx_url: &Url) -> Result<Index> {
        let idx_path = idx_url.to_file_path().map_err(|_| ZdbError::invalid_path("Invalid FTS index URL path".to_string()))?;
        
        // Check if .idx file exists first, then fall back to directory
        if idx_path.exists() {
            if idx_path.is_file() {
                let zip_directory = ZipDirectory::open(idx_path);
                Index::open(Box::new(zip_directory) as Box<dyn tantivy::directory::Directory>)
                    .map_err(|e| ZdbError::general_error(format!("Failed to open packed FTS index: {}", e)))
            } else {
                Index::open_in_dir(&idx_path)
                    .map_err(|e| ZdbError::general_error(format!("Failed to open FTS index directory: {}", e)))
            }
        } else {
            Err(ZdbError::general_error(format!("FTS index not found. Checked for: {}", idx_path.display())))
        }
    }

    
    /// Perform full-text search on the database content
    /// Returns a vector of (score, entry_no, key) tuples for matching entries
    pub fn fts_search(&self, query_str: &str, max_results: usize) -> Result<Vec<(f32, EntryNo, String)>> {
        if let Some(ref fts_index) = self.fts_index {
            // Create a searcher for searching
            let reader = fts_index.reader()
                .map_err(|e| ZdbError::general_error(format!("Failed to create FTS index reader: {}", e)))?;
            let searcher = reader.searcher();
            
            // Get schema fields
            let schema = fts_index.schema();
            let key_field = schema.get_field("key")
                .map_err(|_| ZdbError::general_error("Field 'key' not found in FTS schema".to_string()))?;
            let content_field = schema.get_field("content")
                .map_err(|_| ZdbError::general_error("Field 'content' not found in FTS schema".to_string()))?;
            let entry_no_field = schema.get_field("entry_no")
                .map_err(|_| ZdbError::general_error("Field 'entry_no' not found in FTS schema".to_string()))?;
            
            // Create query parser for the searchable fields  
            let query_parser = QueryParser::for_index(fts_index, vec![key_field, content_field]);
            
            // Parse the search query
            let query = query_parser.parse_query(query_str)
                .map_err(|e| ZdbError::general_error(format!("Failed to parse query '{}': {}", query_str, e)))?;
            
            // Perform the search
            let top_docs = searcher.search(&query, &TopDocs::with_limit(max_results))
                .map_err(|e| ZdbError::general_error(format!("FTS search failed: {}", e)))?;
            
            // Extract results
            let mut results = Vec::new();
            for (score, doc_address) in top_docs {
                // Retrieve the document from the index
                let retrieved_doc = searcher.doc::<TantivyDocument>(doc_address)
                    .map_err(|e| ZdbError::general_error(format!("Failed to retrieve document: {}", e)))?;
                
                // Extract fields from the document
                let entry_no: EntryNo = retrieved_doc
                    .get_first(entry_no_field)
                    .and_then(|v| v.as_u64())
                    .map(|n| n as EntryNo)
                    .ok_or(ZdbError::general_error("Entry number not found in FTS index".to_string()))?;
                let key = retrieved_doc.get_first(key_field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                    
                results.push((score, entry_no, key));
            }
            
            Ok(results)
        } else {
            Err(ZdbError::general_error("Full-text search index is not available".to_string()))
        }
    }
    
    /// Check if full-text search is available (index is loaded and not empty)
    pub fn is_fts_available(&self) -> bool {
        if let Some(ref fts_index) = self.fts_index {
            if let Ok(reader) = fts_index.reader() {
                reader.searcher().num_docs() > 0
            } else {
                false
            }
        } else {
            false
        }
    }

    /// 释放内容库与全部 MDD 分卷缓存的数据块。
    /// 批量扫描（前缀搜索 / 全文搜索）结束后调用，可把常驻内存降回基线；
    /// 后续按需读取会重新解码对应块，仅损失少量重复解压时间。
    pub fn trim_caches(&mut self) {
        self.content_db.clear_caches();
        if let Some(ref mut data_db) = self.data_db {
            data_db.trim_caches();
        }
    }

    /// Check if data database is available (for resources like CSS, images, etc.)
    pub fn is_data_db_available(&self) -> bool {
        self.data_db.is_some()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 普通CSS样式表不产生compact表() {
        // 带 CSS 样式表的词典：token 行不是数字，必须得到空表（而非 Err），
        // 否则 from_url 会把整本词典判为加载失败。
        let css = "body { color: red; }\np { margin: 0; }";
        let table = MdxReader::load_compact_stylesheet(css).unwrap();
        assert!(table.is_empty(), "CSS 样式表不应产生 compact 映射: {:?}", table);
    }

    #[test]
    fn compact三元组正确解析() {
        // 标准 3 行一组：token / prefix / suffix
        let ss = "0\n<b>&lt;</b>\n</b>\n1\n<i>\n</i>";
        let table = MdxReader::load_compact_stylesheet(ss).unwrap();
        assert_eq!(table.len(), 256);
        assert_eq!(table[0].0, "<b><</b>");   // &lt; 已被 htmlescape 解码
        assert_eq!(table[0].1, "</b>");
        assert_eq!(table[1].0, "<i>");
        assert_eq!(table[1].1, "</i>");
    }

    #[test]
    fn CRLF换行与首尾空白被清理() {
        // 很多词典 header 用 \r\n，且 prefix/suffix 两侧带空白
        let ss = " 2 \r\n  <u> \r\n  </u> \r\n";
        let table = MdxReader::load_compact_stylesheet(ss).unwrap();
        assert_eq!(table[2].0, "<u>");
        assert_eq!(table[2].1, "</u>");
    }

    #[test]
    fn 畸形与非数字token被跳过不报错() {
        // 混杂非数字行、超范围 token，都不能让解析失败；合法 token 仍生效
        let ss = "garbage\nX\nY\n300\nbad\nbad\n5\n<code>\n</code>";
        let table = MdxReader::load_compact_stylesheet(ss).unwrap();
        assert_eq!(table.len(), 256);
        assert!(table[5].0.contains("code"));
        // 超范围 token 未写入任何 0..255 槽位
        assert!(table.iter().all(|(p, _)| !p.contains("bad")));
    }

    #[test]
    fn 空样式表返回空表() {
        for s in &["", "\n", "\r\n"] {
            assert!(MdxReader::load_compact_stylesheet(s).unwrap().is_empty());
        }
    }

    #[test]
    fn reformat展开反引号token() {
        let table = vec![
            ("<b>".to_string(), "</b>".to_string()),
            ("<i>".to_string(), "</i>".to_string()),
        ];
        let out = MdxReader::reformat("A`0`粗体`B`1`斜体`", &table).unwrap();
        assert_eq!(out, "A<b>粗体</b>B<i>斜体</i>");
    }

    #[test]
    fn reformat未知token原样保留() {
        let table = vec![("<b>".to_string(), "</b>".to_string())];
        let out = MdxReader::reformat("X`9`未知`Y", &table).unwrap();
        // token 9 越界（表只有 1 项），不得 panic，整段原样保留
        assert_eq!(out, "X`9`未知`Y");
    }
}