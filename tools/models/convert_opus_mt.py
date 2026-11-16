#!/usr/bin/env python3
"""Converts Helsinki-NLP/opus-mt_tiny_rus-eng into the int8 ONNX bundle the app ships.

    python -m venv .venv && .venv/bin/pip install -r tools/models/requirements.txt
    .venv/bin/python tools/models/convert_opus_mt.py --out build/opus-mt-ru-en

Output directory (about 25 MB):

    encoder.onnx        encoder, dynamic int8 (MatMul + Gather)
    decoder.onnx        merged decoder (first step and KV-cache steps in one graph)
    spm.model           SentencePiece model (source and target share it)
    vocab.json          piece -> id table the model was trained with
    config.json         the Marian config (special token ids, sizes)
    MANIFEST.txt        SHA-256 of each file, for pinning in the app build
    LICENSE-opus-mt.txt Apache-2.0 notice

The script also checks the result: the ONNX models must translate a fixed set
of sentences the way PyTorch does (greedy decoding), and it writes
`tokenizer_fixture.json` / `translation_fixture.json` that the Rust crate's
tests compare against (see core/crates/examic-mt/tests).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

MODEL_ID = "Helsinki-NLP/opus-mt_tiny_rus-eng"
# Pin the exact upstream revision, so a re-run produces the same bundle.
MODEL_REVISION = "795b0b2fb13d17b917287421c15e0d70db9ba940"
MAX_NEW_TOKENS = 256

# A spread of lecture/meeting-style Russian: punctuation, numbers, names,
# questions, a long sentence and a fragment, as ASR would produce them.
SENTENCES = [
    "Добрый день, коллеги.",
    "Сегодня мы поговорим о линейной алгебре.",
    "Откройте, пожалуйста, учебник на странице сорок два.",
    "Экзамен состоится в следующий вторник в десять утра.",
    "Кто может объяснить, что такое собственный вектор?",
    "Основной религией в Молдавии является православное христианство.",
    "Амазонка также является самой широкой рекой на Земле.",
    "Я не уверен, что мы успеем закончить проект к пятнице.",
    "Давайте вернёмся к этому вопросу после перерыва.",
    "В 2010 году число студентов выросло на 8%.",
    "Москва — столица России.",
    "Спасибо всем за внимание.",
    "Это не казалось мне имеющим смысл. Конечно, это не было справедливым.",
    "Прежде чем организация сможет стать инновационной, руководители должны создать культуру новаторства.",
    "Ну, в общем, да.",
    "Пожалуйста, запишите домашнее задание.",
    "Функция непрерывна на всём отрезке.",
    "Иван Петрович опоздал на встречу.",
    "Почему этот метод не работает для больших матриц?",
    "Следующая лекция будет посвящена теории вероятностей и математической статистике, "
    "поэтому повторите материал прошлого семестра.",
]


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def export(model_dir: Path, work: Path) -> Path:
    out = work / "export"
    subprocess.run(
        [
            sys.executable, "-m", "optimum.exporters.onnx",
            "--model", str(model_dir),
            "--task", "text2text-generation-with-past",
            "--dtype", "fp32",
            str(out),
        ],
        check=True,
    )
    return out


def quantize(src: Path, dst: Path) -> None:
    from onnxruntime.quantization import QuantType, quantize_dynamic

    # Gather covers the embedding tables, which are most of the weights of a
    # model this small; MatMul covers attention, feed-forward and the output
    # projection. Signed int8 weights are what ARM64 kernels prefer.
    quantize_dynamic(
        str(src),
        str(dst),
        op_types_to_quantize=["MatMul", "Gather"],
        weight_type=QuantType.QInt8,
        per_channel=False,
    )


class OnnxTranslator:
    """Greedy decoding exactly as examic-mt does it, for the parity check."""

    def __init__(self, encoder: Path, decoder: Path, config: dict):
        import onnxruntime as ort

        opts = ort.SessionOptions()
        opts.intra_op_num_threads = 1
        self.encoder = ort.InferenceSession(str(encoder), opts)
        self.decoder = ort.InferenceSession(str(decoder), opts)
        self.config = config
        self.layers = config["decoder_layers"]
        self.heads = config["decoder_attention_heads"]
        self.head_dim = config["d_model"] // self.heads

    def generate(self, input_ids: list[int]) -> list[int]:
        import numpy as np

        ids = np.array([input_ids], dtype=np.int64)
        mask = np.ones_like(ids)
        hidden = self.encoder.run(None, {"input_ids": ids, "attention_mask": mask})[0]
        empty = np.zeros((1, self.heads, 0, self.head_dim), dtype=np.float32)
        past = {}
        for i in range(self.layers):
            for kind in ("decoder", "encoder"):
                for kv in ("key", "value"):
                    past[f"past_key_values.{i}.{kind}.{kv}"] = empty
        token = self.config["decoder_start_token_id"]
        pad = self.config["pad_token_id"]
        eos = self.config["eos_token_id"]
        out: list[int] = []
        for step in range(MAX_NEW_TOKENS):
            feeds = {
                "input_ids": np.array([[token]], dtype=np.int64),
                "encoder_attention_mask": mask,
                "encoder_hidden_states": hidden,
                "use_cache_branch": np.array([step > 0]),
                **past,
            }
            names = [o.name for o in self.decoder.get_outputs()]
            results = dict(zip(names, self.decoder.run(None, feeds)))
            logits = results["logits"][0, -1].copy()
            logits[pad] = -np.inf  # generation_config: bad_words_ids = [[pad]]
            token = int(np.argmax(logits))
            if token == eos:
                break
            out.append(token)
            for i in range(self.layers):
                for kv in ("key", "value"):
                    past[f"past_key_values.{i}.decoder.{kv}"] = results[f"present.{i}.decoder.{kv}"]
                    if step == 0:
                        # Cross-attention keys are computed once, on the first step.
                        past[f"past_key_values.{i}.encoder.{kv}"] = results[f"present.{i}.encoder.{kv}"]
        return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, required=True, help="output directory")
    parser.add_argument("--model", default=None, help="local model directory instead of downloading")
    parser.add_argument("--fixtures", type=Path, default=None,
                        help="also copy the test fixtures here (core/crates/examic-mt/tests/fixtures)")
    parser.add_argument("--min-int8-exact", type=int, default=15,
                        help="int8 translations that must equal PyTorch's word for word (of 20)")
    args = parser.parse_args()

    import torch
    from huggingface_hub import snapshot_download
    from transformers import MarianMTModel, MarianTokenizer

    model_dir = Path(args.model) if args.model else Path(
        snapshot_download(MODEL_ID, revision=MODEL_REVISION,
                          allow_patterns=["*.json", "*.spm", "model.safetensors", "README.md"]))
    config = json.loads((model_dir / "config.json").read_text())
    if (model_dir / "source.spm").read_bytes() != (model_dir / "target.spm").read_bytes():
        raise SystemExit("source.spm and target.spm differ; the app assumes one shared model")

    args.out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        exported = export(model_dir, work)
        quantize(exported / "encoder_model.onnx", args.out / "encoder.onnx")
        # The quantizer does not descend into the If branches of the merged
        # decoder, so the two decoders are quantized first and merged after:
        # their identical int8 weights are then stored once.
        quantize(exported / "decoder_model.onnx", work / "decoder_q.onnx")
        quantize(exported / "decoder_with_past_model.onnx", work / "decoder_with_past_q.onnx")
        from optimum.onnx import merge_decoders

        merge_decoders(work / "decoder_q.onnx", work / "decoder_with_past_q.onnx",
                       save_path=args.out / "decoder.onnx", strict=False)

        # Reference: PyTorch greedy decoding, and the fp32 ONNX graphs.
        tokenizer = MarianTokenizer.from_pretrained(model_dir)
        model = MarianMTModel.from_pretrained(model_dir, torch_dtype=torch.float32).eval()
        fp32 = OnnxTranslator(exported / "encoder_model.onnx", exported / "decoder_model_merged.onnx", config)
        int8 = OnnxTranslator(args.out / "encoder.onnx", args.out / "decoder.onnx", config)

        tokenizer_cases, translation_cases = [], []
        fp32_exact = int8_exact = 0
        for sentence in SENTENCES:
            input_ids = tokenizer(sentence)["input_ids"]
            with torch.no_grad():
                ref = model.generate(torch.tensor([input_ids]), num_beams=1, do_sample=False,
                                     max_new_tokens=MAX_NEW_TOKENS)[0].tolist()
            ref_text = tokenizer.decode(ref, skip_special_tokens=True)
            fp32_text = tokenizer.decode(fp32.generate(input_ids), skip_special_tokens=True)
            int8_ids = int8.generate(input_ids)
            int8_text = tokenizer.decode(int8_ids, skip_special_tokens=True)
            fp32_exact += fp32_text == ref_text
            int8_exact += int8_text == ref_text
            print(f"RU   {sentence}\nPT   {ref_text}\nFP32 {fp32_text}\nINT8 {int8_text}\n")
            tokenizer_cases.append({"text": sentence, "ids": input_ids,
                                    "pieces": tokenizer.convert_ids_to_tokens(input_ids)})
            translation_cases.append({"text": sentence, "output_ids": int8_ids, "translation": int8_text})

    print(f"fp32 ONNX matches PyTorch on {fp32_exact}/{len(SENTENCES)}")
    print(f"int8 ONNX matches PyTorch on {int8_exact}/{len(SENTENCES)}")
    if fp32_exact != len(SENTENCES):
        raise SystemExit("the fp32 export disagrees with PyTorch: the export is broken")
    if int8_exact < args.min_int8_exact:
        raise SystemExit("int8 quantization changed too many translations")

    shutil.copyfile(model_dir / "source.spm", args.out / "spm.model")
    shutil.copyfile(model_dir / "vocab.json", args.out / "vocab.json")
    shutil.copyfile(model_dir / "config.json", args.out / "config.json")
    (args.out / "LICENSE-opus-mt.txt").write_text(
        f"{MODEL_ID} (revision {MODEL_REVISION})\n"
        "Copyright Language Technology Research Group at the University of Helsinki.\n"
        "Licensed under the Apache License, Version 2.0: https://www.apache.org/licenses/LICENSE-2.0\n"
        "Converted to ONNX and quantized to int8 by tools/models/convert_opus_mt.py.\n"
    )
    fixtures = {"tokenizer_fixture.json": tokenizer_cases, "translation_fixture.json": translation_cases}
    for name, cases in fixtures.items():
        (args.out / name).write_text(json.dumps(cases, ensure_ascii=False, indent=1) + "\n")
        if args.fixtures:
            args.fixtures.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(args.out / name, args.fixtures / name)

    shipped = ["encoder.onnx", "decoder.onnx", "spm.model", "vocab.json", "config.json", "LICENSE-opus-mt.txt"]
    manifest = "".join(f"{sha256(args.out / f)}  {f}\n" for f in shipped)
    (args.out / "MANIFEST.txt").write_text(manifest)
    print(manifest)
    total = sum((args.out / f).stat().st_size for f in shipped)
    print(f"bundle: {total / 1e6:.1f} MB in {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
