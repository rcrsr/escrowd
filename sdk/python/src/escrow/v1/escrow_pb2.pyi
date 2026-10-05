from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ChangeKind(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    CHANGE_KIND_UNSPECIFIED: _ClassVar[ChangeKind]
    CHANGE_KIND_CREATE: _ClassVar[ChangeKind]
    CHANGE_KIND_MODIFY: _ClassVar[ChangeKind]
    CHANGE_KIND_DELETE: _ClassVar[ChangeKind]
    CHANGE_KIND_RENAME: _ClassVar[ChangeKind]

class ReadDecision(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    READ_DECISION_UNSPECIFIED: _ClassVar[ReadDecision]
    READ_DECISION_ALLOW: _ClassVar[ReadDecision]
    READ_DECISION_DENY: _ClassVar[ReadDecision]

class Verdict(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    VERDICT_UNSPECIFIED: _ClassVar[Verdict]
    VERDICT_COMMIT: _ClassVar[Verdict]
    VERDICT_DISCARD: _ClassVar[Verdict]
    VERDICT_RETURN: _ClassVar[Verdict]

class OutcomeStatus(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    OUTCOME_STATUS_UNSPECIFIED: _ClassVar[OutcomeStatus]
    OUTCOME_STATUS_COMMITTED: _ClassVar[OutcomeStatus]
    OUTCOME_STATUS_DISCARDED: _ClassVar[OutcomeStatus]
    OUTCOME_STATUS_RETURNED: _ClassVar[OutcomeStatus]
    OUTCOME_STATUS_CONFLICT: _ClassVar[OutcomeStatus]
CHANGE_KIND_UNSPECIFIED: ChangeKind
CHANGE_KIND_CREATE: ChangeKind
CHANGE_KIND_MODIFY: ChangeKind
CHANGE_KIND_DELETE: ChangeKind
CHANGE_KIND_RENAME: ChangeKind
READ_DECISION_UNSPECIFIED: ReadDecision
READ_DECISION_ALLOW: ReadDecision
READ_DECISION_DENY: ReadDecision
VERDICT_UNSPECIFIED: Verdict
VERDICT_COMMIT: Verdict
VERDICT_DISCARD: Verdict
VERDICT_RETURN: Verdict
OUTCOME_STATUS_UNSPECIFIED: OutcomeStatus
OUTCOME_STATUS_COMMITTED: OutcomeStatus
OUTCOME_STATUS_DISCARDED: OutcomeStatus
OUTCOME_STATUS_RETURNED: OutcomeStatus
OUTCOME_STATUS_CONFLICT: OutcomeStatus

class PingRequest(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class PingResponse(_message.Message):
    __slots__ = ("daemon_version", "protocol_version")
    DAEMON_VERSION_FIELD_NUMBER: _ClassVar[int]
    PROTOCOL_VERSION_FIELD_NUMBER: _ClassVar[int]
    daemon_version: str
    protocol_version: int
    def __init__(self, daemon_version: _Optional[str] = ..., protocol_version: _Optional[int] = ...) -> None: ...

class OpenScopeRequest(_message.Message):
    __slots__ = ("name", "labels")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    NAME_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    name: str
    labels: _containers.ScalarMap[str, str]
    def __init__(self, name: _Optional[str] = ..., labels: _Optional[_Mapping[str, str]] = ...) -> None: ...

class OpenScopeResponse(_message.Message):
    __slots__ = ("scope_id", "root", "roots")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    ROOT_FIELD_NUMBER: _ClassVar[int]
    ROOTS_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    root: str
    roots: _containers.RepeatedCompositeFieldContainer[ScopeRoot]
    def __init__(self, scope_id: _Optional[str] = ..., root: _Optional[str] = ..., roots: _Optional[_Iterable[_Union[ScopeRoot, _Mapping]]] = ...) -> None: ...

class ScopeRoot(_message.Message):
    __slots__ = ("path", "view", "direct")
    PATH_FIELD_NUMBER: _ClassVar[int]
    VIEW_FIELD_NUMBER: _ClassVar[int]
    DIRECT_FIELD_NUMBER: _ClassVar[int]
    path: str
    view: str
    direct: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, path: _Optional[str] = ..., view: _Optional[str] = ..., direct: _Optional[_Iterable[str]] = ...) -> None: ...

class CloseScopeRequest(_message.Message):
    __slots__ = ("scope_id",)
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    def __init__(self, scope_id: _Optional[str] = ...) -> None: ...

class Change(_message.Message):
    __slots__ = ("kind", "path", "from_path")
    KIND_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    FROM_PATH_FIELD_NUMBER: _ClassVar[int]
    kind: ChangeKind
    path: str
    from_path: str
    def __init__(self, kind: _Optional[_Union[ChangeKind, str]] = ..., path: _Optional[str] = ..., from_path: _Optional[str] = ...) -> None: ...

class Read(_message.Message):
    __slots__ = ("path", "decision")
    PATH_FIELD_NUMBER: _ClassVar[int]
    DECISION_FIELD_NUMBER: _ClassVar[int]
    path: str
    decision: ReadDecision
    def __init__(self, path: _Optional[str] = ..., decision: _Optional[_Union[ReadDecision, str]] = ...) -> None: ...

class ChangeSet(_message.Message):
    __slots__ = ("scope_id", "changes", "reads", "labels", "diff")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    CHANGES_FIELD_NUMBER: _ClassVar[int]
    READS_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    DIFF_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    changes: _containers.RepeatedCompositeFieldContainer[Change]
    reads: _containers.RepeatedCompositeFieldContainer[Read]
    labels: _containers.ScalarMap[str, str]
    diff: str
    def __init__(self, scope_id: _Optional[str] = ..., changes: _Optional[_Iterable[_Union[Change, _Mapping]]] = ..., reads: _Optional[_Iterable[_Union[Read, _Mapping]]] = ..., labels: _Optional[_Mapping[str, str]] = ..., diff: _Optional[str] = ...) -> None: ...

class GetChangeSetRequest(_message.Message):
    __slots__ = ("scope_id",)
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    def __init__(self, scope_id: _Optional[str] = ...) -> None: ...

class DecideRequest(_message.Message):
    __slots__ = ("scope_id", "verdict", "reasons")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    VERDICT_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    verdict: Verdict
    reasons: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, scope_id: _Optional[str] = ..., verdict: _Optional[_Union[Verdict, str]] = ..., reasons: _Optional[_Iterable[str]] = ...) -> None: ...

class Outcome(_message.Message):
    __slots__ = ("scope_id", "status", "paths", "reasons")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    PATHS_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    status: OutcomeStatus
    paths: _containers.RepeatedScalarFieldContainer[str]
    reasons: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, scope_id: _Optional[str] = ..., status: _Optional[_Union[OutcomeStatus, str]] = ..., paths: _Optional[_Iterable[str]] = ..., reasons: _Optional[_Iterable[str]] = ...) -> None: ...

class SpawnRequest(_message.Message):
    __slots__ = ("scope_id", "argv", "cwd", "env")
    class EnvEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    CWD_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    argv: _containers.RepeatedScalarFieldContainer[str]
    cwd: str
    env: _containers.ScalarMap[str, str]
    def __init__(self, scope_id: _Optional[str] = ..., argv: _Optional[_Iterable[str]] = ..., cwd: _Optional[str] = ..., env: _Optional[_Mapping[str, str]] = ...) -> None: ...

class SpawnEvent(_message.Message):
    __slots__ = ("pid", "exit_code", "error")
    PID_FIELD_NUMBER: _ClassVar[int]
    EXIT_CODE_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    pid: int
    exit_code: int
    error: str
    def __init__(self, pid: _Optional[int] = ..., exit_code: _Optional[int] = ..., error: _Optional[str] = ...) -> None: ...

class SpawnSignal(_message.Message):
    __slots__ = ("signal",)
    SIGNAL_FIELD_NUMBER: _ClassVar[int]
    signal: int
    def __init__(self, signal: _Optional[int] = ...) -> None: ...

class SettleUnscopedRequest(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...
