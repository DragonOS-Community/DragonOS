#!/usr/bin/env python3
"""
DragonOS文档自动翻译工具
Usage:
在DragonOS源码根目录下运行此脚本。

需要先进入docs目录，执行命令安装依赖包。
pip install -r requirements.txt

接着先声明以下变量：
export OPENAI_API_KEY=your_api_key
export OPENAI_MODEL=your_model_name (推荐qwen3的4b以上的)
export OPENAI_BASE_URL=your_openai_base_url
export MAX_WORKERS=your_max_workers (推荐2-20)

然后运行：
python3 tools/doc_translator.py
"""

import os
import re
import hashlib
import json
from pathlib import Path
import sys
import threading
from typing import List, Dict, Tuple
import openai
import datetime
import time

from tqdm import tqdm

# 配置


def get_env_var(name, required=False, default=None):
    """从环境变量获取配置"""
    value = os.getenv(name, default)
    if required and not value:
        raise ValueError(f"环境变量 {name} 未设置")
    return value


CONFIG = {
    "source_dir": "docs",  # 源文档目录
    "target_languages": {
        "en": "English",
    },
    "dirs_exclude": ["_build", "locales"],  # 排除的目录
    "model": get_env_var("OPENAI_MODEL", default="qwen3:4b"),  # 模型名称
    # API地址
    "base_url": get_env_var("OPENAI_BASE_URL", default="http://localhost:11434/v1"),
    "chunk_size": 1000,  # 分块大小(tokens)
    "cache_file": "docs/.translation_cache.json",  # 翻译缓存文件
    "max_workers": int(get_env_var("MAX_WORKERS", default="1")),  # 并行工作数
    # 元数据模板
    "meta_templates": {
        ".rst": (
            ".. note:: AI Translation Notice\n\n"
            "   This document was automatically translated by `{model}` model, for reference only.\n\n"
            "   - Source document: {original_path}\n\n"
            "   - Translation time: {timestamp}\n\n"
            "   - Translation model: `{model}`\n\n"
            "\n   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_\n\n"
        ),
        ".md": (
            ":::{note}\n"
            "**AI Translation Notice**\n\n"
            "This document was automatically translated by `{model}` model, for reference only.\n\n"
            "- Source document: {original_path}\n\n"
            "- Translation time: {timestamp}\n\n"
            "- Translation model: `{model}`\n\n"
            "Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)\n\n"
            ":::\n\n"
        )
    }
}


class LabelManager:
    """管理文档标签和引用"""

    def __init__(self, lang: str):
        self.label_map = {}
        self.prefix = "_translated_label_"
        self.lang = lang

    def register_label(self, original_label: str) -> str:
        """注册新标签并返回映射后的标签"""
        if original_label not in self.label_map:
            new_label = f"{self.prefix}_{original_label}_{self.lang}"
            self.label_map[original_label] = new_label
        return self.label_map[original_label]

    def get_all_labels(self) -> Dict[str, str]:
        """获取所有标签映射"""
        return self.label_map


class DocumentTranslator:
    def __init__(self):
        self._cache_lock = threading.Lock()
        self._cache = self._load_cache()
        self.fail_count = 0
        try:
            self.client = openai.OpenAI(
                base_url=CONFIG["base_url"],
                # 这是故意把key的获取写在这里的。防止哪个二货直接print CONFIG导致key泄露。
                api_key=get_env_var("OPENAI_API_KEY", default="ollama"),
            )
        except Exception as e:
            raise RuntimeError(f"OpenAI客户端初始化失败: {str(e)}")

    def _load_cache(self) -> Dict:
        """加载翻译缓存"""
        if os.path.exists(CONFIG["cache_file"]):
            with open(CONFIG["cache_file"], "r", encoding="utf-8") as f:
                try:
                    return json.load(f)
                except json.JSONDecodeError:
                    pass
        return {}

    def _save_cache(self):
        """保存翻译缓存"""
        with self._cache_lock:
            with open(CONFIG["cache_file"], "w", encoding="utf-8") as f:
                json.dump(self._cache, f, ensure_ascii=False, indent=2)

    def _get_cache_key(self, filepath: str, lang: str) -> str:
        """生成缓存键(包含语言代码)"""
        rel_path = os.path.relpath(filepath, CONFIG["source_dir"])
        return f"{lang}:{rel_path}"

    def _split_into_chunks(self, text: str) -> List[str]:
        """将文本分块"""
        # 按段落分割
        paragraphs = re.split(r"\n\s*\n", text)
        chunks = []
        current_chunk = []
        current_size = 0
        for para in paragraphs:
            para_size = len(para.split())
            if current_size + para_size > CONFIG["chunk_size"] and current_chunk:
                chunks.append("\n\n".join(current_chunk))
                current_chunk = []
                current_size = 0
            current_chunk.append(para)
            current_size += para_size

        if current_chunk:
            chunks.append("\n\n".join(current_chunk))

        return chunks

    def _process_rst_labels(self, text: str, label_manager: LabelManager) -> str:
        """处理reStructuredText标签"""
        def replace_label(match):
            original_label = match.group(1)
            new_label = label_manager.register_label(original_label)
            return f'.. {new_label}:'

        # 处理标签定义
        text = re.sub(r'\.\.\s+_([^:]+):', replace_label, text)
        # 处理标签引用
        text = re.sub(r'(?<!\w)`([^`]+)`(?!\w)',
                      lambda m: f'`{label_manager.register_label(m.group(1))}`',
                      text)
        return text

    def _process_md_labels(self, text: str, label_manager: LabelManager) -> str:
        """处理Markdown标签"""
        # 处理显式标签定义 {#label}
        text = re.sub(r'\{#([^}]+)\}',
                      lambda m: f'{{#{label_manager.register_label(m.group(1))}}}',
                      text)

        # 处理显式标签定义 (label)=
        text = re.sub(r'\(([^)]+)\)=',
                      lambda m: f'({label_manager.register_label(m.group(1))})=',
                      text)

        # 处理标签引用 [text](#label)
        text = re.sub(r'\[([^\]]+)\]\(#([^)]+)\)',
                      lambda m: f'[{m.group(1)}](#{label_manager.register_label(m.group(2))})',
                      text)

        # 处理裸标签引用 #label
        text = re.sub(r'(?<!\w)#([\w-]+)(?!\w)',
                      lambda m: f'#{label_manager.register_label(m.group(1))}',
                      text)

        return text

    def _generate_unique_label_for_lang(self, text: str, lang: str) -> str:
        # 处理标签

        label_manager = LabelManager(lang)
        text = self._process_rst_labels(text, label_manager)
        text = self._process_md_labels(text, label_manager)
        return text

    def _preserve_special_format(self, text: str) -> Tuple[str, Dict]:
        """保留特殊格式"""
        preserved = {}

        # 排除不需要翻译的块
        exclude_blocks = re.findall(
            r'\.\. Note: __EXCLUDE_IN_TRANSLATED_START.*?\.\. Note: __EXCLUDE_IN_TRANSLATED_END',
            text, re.DOTALL)
        for block in exclude_blocks:
            text = text.replace(block, '')

        # 处理多行代码块
        code_blocks = re.findall(r"```.*?\n.*?```", text, re.DOTALL)
        for i, block in enumerate(code_blocks):
            placeholder = f"__CODE_BLOCK_{i}__"
            preserved[placeholder] = block
            text = text.replace(block, placeholder)
        # 处理内联代码块
        inline_code = re.findall(r"`[^`]+`", text)
        for i, code in enumerate(inline_code):
            placeholder = f"__INLINE_CODE_{i}__"
            preserved[placeholder] = code
            text = text.replace(code, placeholder)

        return text, preserved

    def _restore_special_format(self, text: str, preserved: Dict) -> str:
        """恢复特殊格式"""
        # 先恢复内联代码块
        for placeholder, content in preserved.items():
            if placeholder.startswith("__INLINE_CODE_"):
                text = text.replace(placeholder, content)

        # 然后恢复多行代码块
        for placeholder, content in preserved.items():
            if placeholder.startswith("__CODE_BLOCK_"):
                text = text.replace(placeholder, content)

        return text

    def _remove_thinking(self, text: str) -> str:
        """Remove <think> tags from text"""
        return re.sub(r'<think>.*?</think>', '', text, flags=re.DOTALL)

    def _translate_chunk(self, args: Tuple[str, str]) -> str:
        """翻译单个文本块(内部方法，用于并行处理)"""
        chunk, lang = args
        retry = 3
        while retry > 0:
            try:
                lang_name = CONFIG["target_languages"].get(lang, "English")
                prompt = f"你是一个专业的文档翻译助手，请将以下中文技术文档准确翻译成{lang_name}，保持技术术语的正确性和格式不变。"

                # disable qwen3's thinking mode
                if "qwen3" in CONFIG["model"].lower():
                    prompt += "\n/no_think\n"
                    chunk += "\n/no_think\n"

                response = self.client.chat.completions.create(
                    extra_body={"enable_thinking": False},
                    model=CONFIG["model"],
                    messages=[
                        {"role": "system", "content": prompt},
                        {"role": "user", "content": chunk}
                    ],
                    temperature=0.3,
                )
                content = response.choices[0].message.content
                return self._remove_thinking(content)
            except Exception as e:
                retry -= 1
                if retry == 0:
                    print("翻译失败: {e}，放弃重试。")
                    return None

                print(f"翻译出错: {e}, retrying... ({retry})")
                time.sleep(2)

    def translate_text(self, text: str, lang: str) -> str:
        """使用openai接口翻译文本
        Args:
            text: 要翻译的文本
            lang: 目标语言代码
        """
        chunks = self._split_into_chunks(text)
        translated_chunks = []

        for chunk in chunks:
            translated_chunk = self._translate_chunk((chunk, lang))
            if translated_chunk:
                translated_chunks.append(translated_chunk)

        return "\n\n".join(translated_chunks)

    def process_file(self, filepath: str, lang: str = "en"):
        """处理单个文件
        Args:
            filepath: 源文件路径
            lang: 目标语言代码 (默认'en')
        """
        rel_path = os.path.relpath(filepath, CONFIG["source_dir"])
        target_path = os.path.join(
            CONFIG["source_dir"], "locales", lang, rel_path)

        # 检查文件是否已存在且未修改
        cache_key = self._get_cache_key(filepath, lang)
        file_hash = hashlib.md5(open(filepath, "rb").read()).hexdigest()
        target_file_exists = os.path.exists(target_path)
        with self._cache_lock:
            if cache_key in self._cache and self._cache[cache_key]["hash"] == file_hash and target_file_exists:
                print(f"文件未修改，跳过: {rel_path} (语言: {lang})")
                return

        print(f"正在处理: {rel_path}")

        # 读取文件内容
        with open(filepath, "r", encoding="utf-8") as f:
            content = f.read()

        # 保留特殊格式
        content, preserved = self._preserve_special_format(content)

        content = self._generate_unique_label_for_lang(content, lang)

        # 分块翻译
        translated_content = self.translate_text(
            content, lang)
        if not translated_content:
            print(f"翻译失败！{filepath}")
            self.fail_count += 1
            return

        # 恢复特殊格式
        translated_content = self._restore_special_format(
            translated_content, preserved)

        # 创建目标目录
        os.makedirs(os.path.dirname(target_path), exist_ok=True)

        # 写入翻译结果
        with open(target_path, "w", encoding="utf-8") as f:
            # 添加翻译元数据
            file_ext = os.path.splitext(filepath)[1]
            template = CONFIG["meta_templates"].get(file_ext, "")

            if template:
                timestamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
                original_path = os.path.relpath(filepath, CONFIG["source_dir"])
                meta_content = template.format(
                    note="{note}",
                    model=CONFIG["model"],
                    timestamp=timestamp,
                    original_name=os.path.basename(filepath),
                    original_path=original_path
                )
                translated_content = meta_content + translated_content

            f.write(translated_content)
            f.write("\n")

        # 更新缓存
        with self._cache_lock:
            self._cache[cache_key] = {
                "hash": file_hash,
            }

        self._save_cache()

        print(f"文件 {rel_path} 已成功翻译为 {lang} 并保存到 {target_path}")

    def add_language_root_title(self, lang):
        """为每个语言的文档添加标题"""
        lang_root_doc_path = os.path.join(
            CONFIG["source_dir"], "locales", lang, "index.rst")

        if not os.path.exists(lang_root_doc_path):
            raise FileNotFoundError(f"未找到 {lang} 的标题文件: {lang_root_doc_path}")
        print(f"正在为 {CONFIG['target_languages'][lang]} 添加标题...")

        # Read existing content first
        with open(lang_root_doc_path, "r", encoding="utf-8") as f:
            content = f.read()

        lang_v = CONFIG["target_languages"][lang]
        if content.startswith(lang_v):
            print(f"{lang_v} 的标题已存在，跳过...")
            return

        # Then write new content (this clears the file)
        with open(lang_root_doc_path, "w", encoding="utf-8") as f:
            f.write(
                f"{lang_v}\n==========================================\n{content}")

        print(f"标题已添加到 {lang_root_doc_path}")

    def run(self):
        """运行翻译流程"""

        print("Collecting all files...")
        all_files = []
        for root, dirs, files in os.walk(CONFIG["source_dir"], topdown=True):
            # 只在根目录应用排除逻辑
            if root == CONFIG["source_dir"]:
                dirs[:] = [d for d in dirs if d not in CONFIG["dirs_exclude"]]
            for file in files:
                if file.endswith((".rst", ".md")):
                    all_files.append(os.path.join(root, file))
        total_files = len(all_files)
        print(
            f"Total {total_files} files to translate in {len(CONFIG['target_languages'])} languages.")
        total_tasks = total_files * len(CONFIG["target_languages"])

        # 外层进度条：语言
        lang_pbar = tqdm(CONFIG["target_languages"].items(),
                         desc="Overall progress",
                         unit="lang",
                         position=0)

        for lang_k, lang_v in lang_pbar:
            lang_pbar.set_description(f"Translating to {lang_v}")

            # 并行处理文件
            from concurrent.futures import ThreadPoolExecutor, as_completed

            # 包装处理函数便于调试之类的

            def process_file_wrapper(file_path):
                self.process_file(file_path, lang_k)
                return file_path

            # 创建线程池
            with ThreadPoolExecutor(max_workers=CONFIG["max_workers"]) as executor:
                # 提交所有文件处理任务
                futures = [executor.submit(
                    process_file_wrapper, path) for path in all_files]

                # 创建进度条
                file_pbar = tqdm(total=len(all_files),
                                 desc=f"Files in {lang_v}",
                                 unit="file",
                                 position=1,
                                 leave=False)

                # 更新进度条
                for future in as_completed(futures):
                    file_pbar.update(1)
                    future.result()  # 获取结果（如果有异常会在这里抛出）

                file_pbar.close()
            self.add_language_root_title(lang_k)

        lang_pbar.close()
        print(
            f"\n翻译完成！ Succ: {total_tasks-self.fail_count}, Fail: {self.fail_count}")
        


if __name__ == "__main__":
    print("Starting translation process...")
    print("WORKERS: ", CONFIG["max_workers"])
    print("LANGUAGES: ", CONFIG["target_languages"])
    print("SOURCE_DIR: ", CONFIG["source_dir"])
    print("MODEL: ", CONFIG["model"])

    translator = DocumentTranslator()
    translator.run()
    
    if translator.fail_count > 0:
        sys.exit(1)
