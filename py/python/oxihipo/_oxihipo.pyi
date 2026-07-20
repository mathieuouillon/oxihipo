"""Type stubs for the compiled `_oxihipo` extension.

The user-facing, fully-typed API is the pure-Python :class:`oxihipo.Chain`
wrapper; this stub types the low-level compiled reader it delegates to.
"""

from collections.abc import Sequence
from typing import Any, final

import numpy as np
from numpy.typing import NDArray

__all__ = ["Chain", "__version__", "OxihipoError", "CorruptFileError"]

__version__: str

class OxihipoError(Exception): ...
class CorruptFileError(OxihipoError): ...

# (bank, columns) selection; empty columns → all columns of that bank.
_Selection = Sequence[tuple[str, Sequence[str]]]
# Per bank: (name, int64 offsets, [(column, values, inner_len), ...]).
_BankColumns = tuple[str, NDArray[np.int64], list[tuple[str, NDArray[Any], int]]]

@final
class Chain:
    # Constructed via __new__ (a PyO3 #[new]); the class is frozen and cannot
    # be subclassed at runtime.
    def __new__(cls, source: Any) -> "Chain": ...
    @property
    def num_entries(self) -> int: ...
    @property
    def file_count(self) -> int: ...
    @property
    def files(self) -> list[str]: ...
    def __len__(self) -> int: ...
    def __contains__(self, bank: str, /) -> bool: ...
    def keys(self, recursive: bool = ...) -> list[str]: ...
    def columns(self, bank: str) -> list[str]: ...
    def typenames(self) -> dict[str, str]: ...
    def read_columns(
        self,
        selection: _Selection,
        entry_start: int | None = ...,
        entry_stop: int | None = ...,
        threads: int = ...,
    ) -> list[_BankColumns]: ...
    def filtered(
        self,
        require: Sequence[str] | None = ...,
        record_tag: Sequence[int] | None = ...,
        event_tag: Sequence[int] | None = ...,
        event_tag_any: int | None = ...,
    ) -> "Chain": ...
    def skim(self, dst: str, compression: str = ...) -> dict[str, int]: ...
    def record_spans(self) -> list[tuple[int, int, int, int]]: ...
    def record_decompressed_sizes(self) -> list[int]: ...
