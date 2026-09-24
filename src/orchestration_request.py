"""Immutable adapter metadata; canonical history is referenced, never copied."""

from dataclasses import asdict, dataclass
import json
import re

from .orchestration_state import identifier, integer, text
from .orchestration_wire import validate_type


@dataclass(frozen=True)
class RunRequest:
    turn_id: str
    query: str
    history_cutoff: int
    history_hash: str

    def __post_init__(self):
        identifier(self.turn_id)
        validate_type(self.turn_id, "id")
        text(self.query, 2048)
        validate_type(self.query, "query")
        integer(self.history_cutoff, 0, 2**53 - 1)
        if type(self.history_hash) is not str or not re.fullmatch(
            "[a-f0-9]{64}", self.history_hash
        ):
            raise ValueError("Invalid request fingerprint")

    def encode(self):
        self.__post_init__()
        return json.dumps(
            asdict(self), sort_keys=True, ensure_ascii=False, separators=(",", ":")
        )

    @classmethod
    def decode(cls, value):
        try:
            if type(value) is not str or len(value.encode()) > 4096:
                raise ValueError
            result = cls(**json.loads(value))
            if result.encode() != value:
                raise ValueError
            return result
        except (ValueError, TypeError, KeyError):
            raise ValueError("Invalid adapter request") from None
