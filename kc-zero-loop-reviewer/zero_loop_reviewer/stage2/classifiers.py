"""Local code classifiers for the RALE "Code" view (Stage 2C).

Two implementations, in order of preference:

* :class:`TransformersCodeClassifier` — the retained CodeBERT-125M /
  CodeGPT-124M pair.  Both are *pre-trained* checkpoints with no defect head, so
  the score is an unsupervised likelihood anomaly: the mean per-token negative
  log-likelihood of the source under the model, mapped through a logistic.  It
  is a real model score, not a classifier's probability, and it is labelled as
  such in ``code_view``'s detail.  ``transformers`` and ``torch`` are imported
  lazily; without them (or without the weights cached locally) the class raises
  :class:`ToolError` and the RALE Code view reports ``unavailable``.  A model
  score is **never** invented.

* :class:`HashingCodeClassifier` — a deterministic, dependency-free baseline
  over hand-written lexical features.  It exists so tests and offline installs
  have a reproducible comparator; it is not a model and never claims to be one.

``default_classifier`` only returns the transformer classifier when it is
actually usable, and ``None`` otherwise.
"""

from __future__ import annotations

import math
import re
from pathlib import Path
from typing import Any, Protocol, runtime_checkable

from ..config import Settings
from ..errors import ToolError

CODEBERT_MODEL = "microsoft/codebert-base"
CODEGPT_MODEL = "microsoft/CodeGPT-small-py"

#: Per-token NLL at which the logistic anomaly score crosses 0.5 (e^-6 ~ 2.5e-3
#: per-token probability).  Fixed so the mapping is reproducible.
NLL_PIVOT = 6.0
#: Mask every Nth token when scoring a masked-LM checkpoint.
MASK_STRIDE = 8


@runtime_checkable
class LocalCodeClassifier(Protocol):
    """A source -> [0, 1] defect-likelihood scorer."""

    name: str

    def score(self, source: str, path: str) -> float: ...


# --------------------------------------------------------------------------- #
# Deterministic baseline
# --------------------------------------------------------------------------- #

#: (regex, weight) — weights are combined with a probabilistic OR, so the score
#: is monotone in the number of distinct suspicious constructs and bounded by 1.
_FEATURES: tuple[tuple[str, float], ...] = (
    (r"\beval\s*\(", 0.85),
    (r"\bexec\s*\(", 0.85),
    (r"shell\s*=\s*True", 0.80),
    (r"\bos\.system\s*\(", 0.80),
    (r"\bos\.popen\s*\(", 0.75),
    (r"\bpickle\.loads?\s*\(", 0.70),
    (r"\byaml\.load\s*\(", 0.60),
    (r"verify\s*=\s*False", 0.60),
    (r"except\s+\w[^:\n]*:\s*\n\s*pass\b", 0.60),
    (r"except\s*:", 0.55),
    (r"while\s+(True|1)\s*:", 0.50),
    (r"\bassert\s*\(\s*[^)\n]*,", 0.50),
    (r"0\.0\.0\.0", 0.40),
    (r"\bsubprocess\b", 0.30),
    (r"#\s*(TODO|FIXME)", 0.20),
)

_COMPILED_FEATURES: tuple[tuple[re.Pattern[str], float], ...] = tuple(
    (re.compile(pattern, re.MULTILINE), weight) for pattern, weight in _FEATURES
)


class HashingCodeClassifier:
    """Deterministic lexical baseline.  No randomness, no I/O, no model."""

    name = "hashing-linear"

    def __init__(self, *, threshold: float = 0.5) -> None:
        self.threshold = float(threshold)

    def features(self, source: str) -> dict[str, float]:
        return {pattern.pattern: weight for pattern, weight in _COMPILED_FEATURES if pattern.search(source)}

    def score(self, source: str, path: str) -> float:
        keep = 1.0
        for weight in self.features(source).values():
            keep *= 1.0 - weight
        score = 1.0 - keep
        return max(0.0, min(1.0, round(score, 6)))


# --------------------------------------------------------------------------- #
# Transformer pair (lazy)
# --------------------------------------------------------------------------- #


class TransformersCodeClassifier:
    """CodeBERT/CodeGPT likelihood-anomaly scorer.

    ``model_name`` may be an HF id or a local directory.  Weights are only ever
    loaded with ``local_files_only=True`` so a review never downloads anything.
    """

    def __init__(
        self,
        model_name: str = CODEBERT_MODEL,
        *,
        device: str = "cpu",
        max_tokens: int = 512,
    ) -> None:
        self.model_name = str(model_name)
        self.name = f"transformers:{self.model_name}"
        self.device = device
        self.max_tokens = max(16, int(max_tokens))
        self._loaded: tuple[Any, Any, Any, str] | None = None

    # -- availability -----------------------------------------------------
    def available(self) -> tuple[bool, str]:
        try:
            self._load()
        except ToolError as exc:
            return False, exc.message
        return True, f"{self.name} loaded locally"

    def _load(self) -> tuple[Any, Any, Any, str]:
        if self._loaded is not None:
            return self._loaded
        try:
            import torch  # noqa: F401  (lazy: absent on most installs)
            from transformers import AutoModelForCausalLM, AutoModelForMaskedLM, AutoTokenizer
        except ImportError as exc:
            raise ToolError(
                "transformers+torch are not importable; the local code classifier is unavailable",
                detail=str(exc),
            ) from None
        try:
            tokenizer = AutoTokenizer.from_pretrained(self.model_name, local_files_only=True)
        except Exception as exc:  # transformers raises many types; all mean "unusable"
            raise ToolError(
                f"tokenizer for {self.model_name} is not available offline",
                detail=str(exc),
            ) from None
        model = None
        kind = ""
        for loader, label in ((AutoModelForCausalLM, "causal"), (AutoModelForMaskedLM, "masked")):
            try:
                model = loader.from_pretrained(self.model_name, local_files_only=True)
            except Exception:
                model = None
                continue
            kind = label
            break
        if model is None or not kind:
            raise ToolError(
                f"weights for {self.model_name} are not available offline",
                detail="neither a causal-LM nor a masked-LM checkpoint could be loaded",
            )
        try:
            model.eval()
            model.to(self.device)
        except Exception as exc:
            raise ToolError(f"could not place {self.model_name} on {self.device}", detail=str(exc)) from None
        self._loaded = (torch, model, tokenizer, kind)
        return self._loaded

    # -- scoring ----------------------------------------------------------
    def score(self, source: str, path: str) -> float:
        if not source.strip():
            raise ToolError("refusing to score an empty source with a language model")
        torch, model, tokenizer, kind = self._load()
        try:
            encoded = tokenizer(
                source[: max(4000, self.max_tokens * 8)],
                return_tensors="pt",
                truncation=True,
                max_length=self.max_tokens,
            )
            input_ids = encoded["input_ids"].to(self.device)
            if input_ids.shape[1] < 2:
                raise ToolError("source tokenises to fewer than two tokens")
            if kind == "causal":
                with torch.no_grad():
                    logits = model(input_ids=input_ids).logits[:, :-1, :]
                    labels = input_ids[:, 1:]
                    nll = torch.nn.functional.cross_entropy(
                        logits.reshape(-1, logits.shape[-1]),
                        labels.reshape(-1),
                        reduction="mean",
                    )
            else:
                masked = input_ids.clone()
                positions = list(range(1, input_ids.shape[1], MASK_STRIDE)) or [0]
                mask_id = _as_int(getattr(tokenizer, "mask_token_id", None))
                if mask_id is None:
                    raise ToolError(f"{self.model_name} has no mask token; cannot score masked-LM likelihood")
                for pos in positions:
                    masked[0, pos] = mask_id
                with torch.no_grad():
                    logits = model(input_ids=masked).logits
                    selected = logits[0, positions, :]
                    labels = input_ids[0, positions]
                    nll = torch.nn.functional.cross_entropy(selected, labels, reduction="mean")
            value = float(nll.item())
        except ToolError:
            raise
        except Exception as exc:
            raise ToolError(f"scoring with {self.model_name} failed", detail=str(exc)) from None
        if not math.isfinite(value):
            raise ToolError(f"{self.model_name} produced a non-finite loss; refusing to report a score")
        # Logistic map: NLL_PIVOT -> 0.5, monotone, bounded by (0, 1).
        return round(1.0 / (1.0 + math.exp(-(value - NLL_PIVOT))), 6)


def _as_int(value: Any) -> int | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


# --------------------------------------------------------------------------- #
# Selection
# --------------------------------------------------------------------------- #


def default_classifier(settings: Settings) -> LocalCodeClassifier | None:
    """The transformer pair when it is usable locally, otherwise ``None``.

    ``settings.paths.state_dir/models/code-classifier`` is used when present so
    an operator can pin a local checkpoint without touching this module.
    """
    candidate = Path(settings.paths.state_dir) / "models" / "code-classifier"
    model_name = str(candidate) if candidate.is_dir() else CODEBERT_MODEL
    classifier = TransformersCodeClassifier(model_name)
    ok, _detail = classifier.available()
    return classifier if ok else None
