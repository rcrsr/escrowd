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

class Tier(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    TIER_UNSPECIFIED: _ClassVar[Tier]
    TIER_SOFTWARE: _ClassVar[Tier]
    TIER_LLM: _ClassVar[Tier]
    TIER_HUMAN: _ClassVar[Tier]

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
    OUTCOME_STATUS_HELD: _ClassVar[OutcomeStatus]
CHANGE_KIND_UNSPECIFIED: ChangeKind
CHANGE_KIND_CREATE: ChangeKind
CHANGE_KIND_MODIFY: ChangeKind
CHANGE_KIND_DELETE: ChangeKind
CHANGE_KIND_RENAME: ChangeKind
READ_DECISION_UNSPECIFIED: ReadDecision
READ_DECISION_ALLOW: ReadDecision
READ_DECISION_DENY: ReadDecision
TIER_UNSPECIFIED: Tier
TIER_SOFTWARE: Tier
TIER_LLM: Tier
TIER_HUMAN: Tier
VERDICT_UNSPECIFIED: Verdict
VERDICT_COMMIT: Verdict
VERDICT_DISCARD: Verdict
VERDICT_RETURN: Verdict
OUTCOME_STATUS_UNSPECIFIED: OutcomeStatus
OUTCOME_STATUS_COMMITTED: OutcomeStatus
OUTCOME_STATUS_DISCARDED: OutcomeStatus
OUTCOME_STATUS_RETURNED: OutcomeStatus
OUTCOME_STATUS_CONFLICT: OutcomeStatus
OUTCOME_STATUS_HELD: OutcomeStatus

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
    __slots__ = ("name", "labels", "session")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    NAME_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    SESSION_FIELD_NUMBER: _ClassVar[int]
    name: str
    labels: _containers.ScalarMap[str, str]
    session: str
    def __init__(self, name: _Optional[str] = ..., labels: _Optional[_Mapping[str, str]] = ..., session: _Optional[str] = ...) -> None: ...

class OpenScopeResponse(_message.Message):
    __slots__ = ("scope_id", "root", "roots", "token")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    ROOT_FIELD_NUMBER: _ClassVar[int]
    ROOTS_FIELD_NUMBER: _ClassVar[int]
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    root: str
    roots: _containers.RepeatedCompositeFieldContainer[ScopeRoot]
    token: str
    def __init__(self, scope_id: _Optional[str] = ..., root: _Optional[str] = ..., roots: _Optional[_Iterable[_Union[ScopeRoot, _Mapping]]] = ..., token: _Optional[str] = ...) -> None: ...

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
    __slots__ = ("scope_id", "token")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    token: str
    def __init__(self, scope_id: _Optional[str] = ..., token: _Optional[str] = ...) -> None: ...

class CloseScopeResponse(_message.Message):
    __slots__ = ("change_set",)
    CHANGE_SET_FIELD_NUMBER: _ClassVar[int]
    change_set: ChangeSet
    def __init__(self, change_set: _Optional[_Union[ChangeSet, _Mapping]] = ...) -> None: ...

class Change(_message.Message):
    __slots__ = ("kind", "path", "from_path", "writers")
    KIND_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    FROM_PATH_FIELD_NUMBER: _ClassVar[int]
    WRITERS_FIELD_NUMBER: _ClassVar[int]
    kind: ChangeKind
    path: str
    from_path: str
    writers: _containers.RepeatedScalarFieldContainer[int]
    def __init__(self, kind: _Optional[_Union[ChangeKind, str]] = ..., path: _Optional[str] = ..., from_path: _Optional[str] = ..., writers: _Optional[_Iterable[int]] = ...) -> None: ...

class Process(_message.Message):
    __slots__ = ("id", "pid", "program", "dev", "ino", "args", "parent")
    ID_FIELD_NUMBER: _ClassVar[int]
    PID_FIELD_NUMBER: _ClassVar[int]
    PROGRAM_FIELD_NUMBER: _ClassVar[int]
    DEV_FIELD_NUMBER: _ClassVar[int]
    INO_FIELD_NUMBER: _ClassVar[int]
    ARGS_FIELD_NUMBER: _ClassVar[int]
    PARENT_FIELD_NUMBER: _ClassVar[int]
    id: int
    pid: int
    program: str
    dev: int
    ino: int
    args: _containers.RepeatedScalarFieldContainer[str]
    parent: int
    def __init__(self, id: _Optional[int] = ..., pid: _Optional[int] = ..., program: _Optional[str] = ..., dev: _Optional[int] = ..., ino: _Optional[int] = ..., args: _Optional[_Iterable[str]] = ..., parent: _Optional[int] = ...) -> None: ...

class Read(_message.Message):
    __slots__ = ("path", "decision")
    PATH_FIELD_NUMBER: _ClassVar[int]
    DECISION_FIELD_NUMBER: _ClassVar[int]
    path: str
    decision: ReadDecision
    def __init__(self, path: _Optional[str] = ..., decision: _Optional[_Union[ReadDecision, str]] = ...) -> None: ...

class ChangeSet(_message.Message):
    __slots__ = ("scope_id", "changes", "reads", "labels", "diff", "unscoped_ops", "review", "processes")
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
    UNSCOPED_OPS_FIELD_NUMBER: _ClassVar[int]
    REVIEW_FIELD_NUMBER: _ClassVar[int]
    PROCESSES_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    changes: _containers.RepeatedCompositeFieldContainer[Change]
    reads: _containers.RepeatedCompositeFieldContainer[Read]
    labels: _containers.ScalarMap[str, str]
    diff: str
    unscoped_ops: int
    review: Review
    processes: _containers.RepeatedCompositeFieldContainer[Process]
    def __init__(self, scope_id: _Optional[str] = ..., changes: _Optional[_Iterable[_Union[Change, _Mapping]]] = ..., reads: _Optional[_Iterable[_Union[Read, _Mapping]]] = ..., labels: _Optional[_Mapping[str, str]] = ..., diff: _Optional[str] = ..., unscoped_ops: _Optional[int] = ..., review: _Optional[_Union[Review, _Mapping]] = ..., processes: _Optional[_Iterable[_Union[Process, _Mapping]]] = ...) -> None: ...

class Review(_message.Message):
    __slots__ = ("verdict", "reasons", "tiers", "wait_required")
    VERDICT_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    TIERS_FIELD_NUMBER: _ClassVar[int]
    WAIT_REQUIRED_FIELD_NUMBER: _ClassVar[int]
    verdict: Verdict
    reasons: _containers.RepeatedScalarFieldContainer[str]
    tiers: _containers.RepeatedScalarFieldContainer[Tier]
    wait_required: bool
    def __init__(self, verdict: _Optional[_Union[Verdict, str]] = ..., reasons: _Optional[_Iterable[str]] = ..., tiers: _Optional[_Iterable[_Union[Tier, str]]] = ..., wait_required: _Optional[bool] = ...) -> None: ...

class GetChangeSetRequest(_message.Message):
    __slots__ = ("scope_id",)
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    def __init__(self, scope_id: _Optional[str] = ...) -> None: ...

class GetChangeSetResponse(_message.Message):
    __slots__ = ("change_set",)
    CHANGE_SET_FIELD_NUMBER: _ClassVar[int]
    change_set: ChangeSet
    def __init__(self, change_set: _Optional[_Union[ChangeSet, _Mapping]] = ...) -> None: ...

class DecideRequest(_message.Message):
    __slots__ = ("scope_id", "verdict", "reasons", "token", "wait")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    VERDICT_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    WAIT_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    verdict: Verdict
    reasons: _containers.RepeatedScalarFieldContainer[str]
    token: str
    wait: bool
    def __init__(self, scope_id: _Optional[str] = ..., verdict: _Optional[_Union[Verdict, str]] = ..., reasons: _Optional[_Iterable[str]] = ..., token: _Optional[str] = ..., wait: _Optional[bool] = ...) -> None: ...

class DecideResponse(_message.Message):
    __slots__ = ("outcome",)
    OUTCOME_FIELD_NUMBER: _ClassVar[int]
    outcome: Outcome
    def __init__(self, outcome: _Optional[_Union[Outcome, _Mapping]] = ...) -> None: ...

class Outcome(_message.Message):
    __slots__ = ("scope_id", "status", "paths", "reasons", "reopened", "tiers", "wait")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    PATHS_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    REOPENED_FIELD_NUMBER: _ClassVar[int]
    TIERS_FIELD_NUMBER: _ClassVar[int]
    WAIT_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    status: OutcomeStatus
    paths: _containers.RepeatedScalarFieldContainer[str]
    reasons: _containers.RepeatedScalarFieldContainer[str]
    reopened: bool
    tiers: _containers.RepeatedScalarFieldContainer[Tier]
    wait: bool
    def __init__(self, scope_id: _Optional[str] = ..., status: _Optional[_Union[OutcomeStatus, str]] = ..., paths: _Optional[_Iterable[str]] = ..., reasons: _Optional[_Iterable[str]] = ..., reopened: _Optional[bool] = ..., tiers: _Optional[_Iterable[_Union[Tier, str]]] = ..., wait: _Optional[bool] = ...) -> None: ...

class SpawnRequest(_message.Message):
    __slots__ = ("scope_id", "argv", "cwd", "env", "token")
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
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    argv: _containers.RepeatedScalarFieldContainer[str]
    cwd: str
    env: _containers.ScalarMap[str, str]
    token: str
    def __init__(self, scope_id: _Optional[str] = ..., argv: _Optional[_Iterable[str]] = ..., cwd: _Optional[str] = ..., env: _Optional[_Mapping[str, str]] = ..., token: _Optional[str] = ...) -> None: ...

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

class SettleUnscopedResponse(_message.Message):
    __slots__ = ("change_set",)
    CHANGE_SET_FIELD_NUMBER: _ClassVar[int]
    change_set: ChangeSet
    def __init__(self, change_set: _Optional[_Union[ChangeSet, _Mapping]] = ...) -> None: ...

class AwaitDecisionRequest(_message.Message):
    __slots__ = ("scope_id", "token")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    token: str
    def __init__(self, scope_id: _Optional[str] = ..., token: _Optional[str] = ...) -> None: ...

class AwaitDecisionResponse(_message.Message):
    __slots__ = ("outcome",)
    OUTCOME_FIELD_NUMBER: _ClassVar[int]
    outcome: Outcome
    def __init__(self, outcome: _Optional[_Union[Outcome, _Mapping]] = ...) -> None: ...

class TierReview(_message.Message):
    __slots__ = ("tier", "verdict", "reasons", "override")
    TIER_FIELD_NUMBER: _ClassVar[int]
    VERDICT_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    OVERRIDE_FIELD_NUMBER: _ClassVar[int]
    tier: Tier
    verdict: Verdict
    reasons: _containers.RepeatedScalarFieldContainer[str]
    override: bool
    def __init__(self, tier: _Optional[_Union[Tier, str]] = ..., verdict: _Optional[_Union[Verdict, str]] = ..., reasons: _Optional[_Iterable[str]] = ..., override: _Optional[bool] = ...) -> None: ...

class HeldScope(_message.Message):
    __slots__ = ("scope_id", "name", "labels", "session", "tiers", "wait", "verdict", "reviews", "held_at_ms")
    class LabelsEntry(_message.Message):
        __slots__ = ("key", "value")
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    NAME_FIELD_NUMBER: _ClassVar[int]
    LABELS_FIELD_NUMBER: _ClassVar[int]
    SESSION_FIELD_NUMBER: _ClassVar[int]
    TIERS_FIELD_NUMBER: _ClassVar[int]
    WAIT_FIELD_NUMBER: _ClassVar[int]
    VERDICT_FIELD_NUMBER: _ClassVar[int]
    REVIEWS_FIELD_NUMBER: _ClassVar[int]
    HELD_AT_MS_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    name: str
    labels: _containers.ScalarMap[str, str]
    session: str
    tiers: _containers.RepeatedScalarFieldContainer[Tier]
    wait: bool
    verdict: Verdict
    reviews: _containers.RepeatedCompositeFieldContainer[TierReview]
    held_at_ms: int
    def __init__(self, scope_id: _Optional[str] = ..., name: _Optional[str] = ..., labels: _Optional[_Mapping[str, str]] = ..., session: _Optional[str] = ..., tiers: _Optional[_Iterable[_Union[Tier, str]]] = ..., wait: _Optional[bool] = ..., verdict: _Optional[_Union[Verdict, str]] = ..., reviews: _Optional[_Iterable[_Union[TierReview, _Mapping]]] = ..., held_at_ms: _Optional[int] = ...) -> None: ...

class ListHeldRequest(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class ListHeldResponse(_message.Message):
    __slots__ = ("scopes",)
    SCOPES_FIELD_NUMBER: _ClassVar[int]
    scopes: _containers.RepeatedCompositeFieldContainer[HeldScope]
    def __init__(self, scopes: _Optional[_Iterable[_Union[HeldScope, _Mapping]]] = ...) -> None: ...

class GetHeldRequest(_message.Message):
    __slots__ = ("scope_id",)
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    def __init__(self, scope_id: _Optional[str] = ...) -> None: ...

class GetHeldResponse(_message.Message):
    __slots__ = ("held", "change_set", "history")
    HELD_FIELD_NUMBER: _ClassVar[int]
    CHANGE_SET_FIELD_NUMBER: _ClassVar[int]
    HISTORY_FIELD_NUMBER: _ClassVar[int]
    held: HeldScope
    change_set: ChangeSet
    history: _containers.RepeatedCompositeFieldContainer[Decided]
    def __init__(self, held: _Optional[_Union[HeldScope, _Mapping]] = ..., change_set: _Optional[_Union[ChangeSet, _Mapping]] = ..., history: _Optional[_Iterable[_Union[Decided, _Mapping]]] = ...) -> None: ...

class Decided(_message.Message):
    __slots__ = ("scope_id", "name", "change_set", "outcome", "reviews", "decided_at_ms")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    NAME_FIELD_NUMBER: _ClassVar[int]
    CHANGE_SET_FIELD_NUMBER: _ClassVar[int]
    OUTCOME_FIELD_NUMBER: _ClassVar[int]
    REVIEWS_FIELD_NUMBER: _ClassVar[int]
    DECIDED_AT_MS_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    name: str
    change_set: ChangeSet
    outcome: Outcome
    reviews: _containers.RepeatedCompositeFieldContainer[TierReview]
    decided_at_ms: int
    def __init__(self, scope_id: _Optional[str] = ..., name: _Optional[str] = ..., change_set: _Optional[_Union[ChangeSet, _Mapping]] = ..., outcome: _Optional[_Union[Outcome, _Mapping]] = ..., reviews: _Optional[_Iterable[_Union[TierReview, _Mapping]]] = ..., decided_at_ms: _Optional[int] = ...) -> None: ...

class ReviewRequest(_message.Message):
    __slots__ = ("scope_id", "tier", "verdict", "reasons", "override")
    SCOPE_ID_FIELD_NUMBER: _ClassVar[int]
    TIER_FIELD_NUMBER: _ClassVar[int]
    VERDICT_FIELD_NUMBER: _ClassVar[int]
    REASONS_FIELD_NUMBER: _ClassVar[int]
    OVERRIDE_FIELD_NUMBER: _ClassVar[int]
    scope_id: str
    tier: Tier
    verdict: Verdict
    reasons: _containers.RepeatedScalarFieldContainer[str]
    override: bool
    def __init__(self, scope_id: _Optional[str] = ..., tier: _Optional[_Union[Tier, str]] = ..., verdict: _Optional[_Union[Verdict, str]] = ..., reasons: _Optional[_Iterable[str]] = ..., override: _Optional[bool] = ...) -> None: ...

class ReviewResponse(_message.Message):
    __slots__ = ("outcome",)
    OUTCOME_FIELD_NUMBER: _ClassVar[int]
    outcome: Outcome
    def __init__(self, outcome: _Optional[_Union[Outcome, _Mapping]] = ...) -> None: ...
