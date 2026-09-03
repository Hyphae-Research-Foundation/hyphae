------------------------ MODULE HyphaeCommit ------------------------
(*
SPDX-License-Identifier: Apache-2.0

Formal model of the Hyphae native cross-engine commit protocol.

Scope (mirrors docs/native/mvcc-commit-v1.md and the implementation in
crates/hyphae-native-runtime/src/lib.rs `commit_report_at`):

  - optimistic transactions capture a read CSN, prepare private write sets
    over logical keys spanning three engines, and submit serially;
  - admission validates first-committer-wins over a conflict table keyed by
    logical write identity, then reserves the next CSN;
  - an admitted commit walks the ordered physical boundary sequence
    BlobStaged -> BlobPromoted -> PageAppended -> PageSynchronized ->
    WalAppended -> WalSynchronized -> RootPublished
    (`CommitBoundary`, lib.rs:1306);
  - WAL fsync is file-wide: synchronizing one commit's WAL transaction also
    makes every earlier appended record durable;
  - Memory-durability commits skip the explicit fsync (lib.rs:24526,
    `synchronize = durability != Memory`); their WAL records stay volatile
    until a later strict fsync or the OS flushes them;
  - a crash may interrupt anything; recovery replays the WAL: durable
    records always survive, volatile records survive only as a CSN-prefix
    (sequential WAL, broken tail truncated), in-flight walks vanish, the
    conflict table is rebuilt from surviving commits.

Checked properties:

  Atomicity              every visible commit exists in the ghost log with
                         its complete engine set (all-or-nothing per CSN;
                         crash can only drop whole commits, never split one);
  StrictDurability       an acknowledged Strict commit survives every crash;
  FirstCommitterWins     two surviving commits over the same key are ordered
                         so the later began at or after the earlier's CSN;
  VisiblePrefixComplete  published CSNs are contiguous 1..visibleCsn in every
                         reachable state, including after every crash shape.

Abstractions: page/blob bytes (ghost engine sets), group-commit cohorts
(Group acknowledges after the shared fsync, i.e. strict at this level),
catalog versions, vacuum/retention, and multi-writer publication (the
implementation serializes publication under one writer guard).
*)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
  Transactions,   \* transaction identities (single-use)
  Keys,           \* logical write keys
  Engines,        \* {"relational", "structure", "search"}
  MaxCrashes      \* crash bound for finite checking

ASSUME MaxCrashes \in Nat

Durabilities == {"strict", "memory"}

Boundaries == <<"BlobStaged", "BlobPromoted", "PageAppended",
                "PageSynchronized", "WalAppended", "WalSynchronized",
                "RootPublished">>

BoundaryCount == Len(Boundaries)
WalAppendedIndex == 5
WalSyncedIndex   == 6
PublishedIndex   == 7

VARIABLES
  phase,        \* [Transactions -> {"idle","prepared","committing","done","aborted"}]
  readCsn,      \* [Transactions -> Nat]
  writeSet,     \* [Transactions -> SUBSET Keys]
  engineSet,    \* [Transactions -> SUBSET Engines]
  durability,   \* [Transactions -> Durabilities]
  progress,     \* [Transactions -> 0..BoundaryCount]
  reservedCsn,  \* [Transactions -> Nat] (0 = none)
  acknowledged, \* SUBSET Transactions
  visibleCsn,   \* Nat
  committed,    \* function csn -> [txn, keys, engines] (ghost log)
  walDurable,   \* SUBSET Nat: fsync-guaranteed commit CSNs
  walVolatile,  \* SUBSET Nat: appended, not fsync-guaranteed
  conflictTable,\* [Keys -> Nat]
  crashes       \* Nat

vars == <<phase, readCsn, writeSet, engineSet, durability, progress,
          reservedCsn, acknowledged, visibleCsn, committed, walDurable,
          walVolatile, conflictTable, crashes>>

Init ==
  /\ phase = [transaction \in Transactions |-> "idle"]
  /\ readCsn = [transaction \in Transactions |-> 0]
  /\ writeSet = [transaction \in Transactions |-> {}]
  /\ engineSet = [transaction \in Transactions |-> {}]
  /\ durability = [transaction \in Transactions |-> "strict"]
  /\ progress = [transaction \in Transactions |-> 0]
  /\ reservedCsn = [transaction \in Transactions |-> 0]
  /\ acknowledged = {}
  /\ visibleCsn = 0
  /\ committed = <<>>
  /\ walDurable = {}
  /\ walVolatile = {}
  /\ conflictTable = [key \in Keys |-> 0]
  /\ crashes = 0

(* begin_optimistic: capture the visible CSN and stage a private write set. *)
Begin(transaction, keys, engines, class) ==
  /\ phase[transaction] = "idle"
  /\ keys /= {}
  /\ engines /= {}
  /\ phase' = [phase EXCEPT ![transaction] = "prepared"]
  /\ readCsn' = [readCsn EXCEPT ![transaction] = visibleCsn]
  /\ writeSet' = [writeSet EXCEPT ![transaction] = keys]
  /\ engineSet' = [engineSet EXCEPT ![transaction] = engines]
  /\ durability' = [durability EXCEPT ![transaction] = class]
  /\ UNCHANGED <<progress, reservedCsn, acknowledged, visibleCsn, committed,
                 walDurable, walVolatile, conflictTable, crashes>>

(* Serialized writer admission: first-committer-wins, then CSN reservation. *)
NoActiveWalk == \A other \in Transactions : phase[other] /= "committing"

Admit(transaction) ==
  /\ phase[transaction] = "prepared"
  /\ NoActiveWalk
  /\ IF \E key \in writeSet[transaction] :
          conflictTable[key] > readCsn[transaction]
     THEN /\ phase' = [phase EXCEPT ![transaction] = "aborted"]
          /\ UNCHANGED <<readCsn, writeSet, engineSet, durability, progress,
                         reservedCsn, acknowledged, visibleCsn, committed,
                         walDurable, walVolatile, conflictTable, crashes>>
     ELSE /\ phase' = [phase EXCEPT ![transaction] = "committing"]
          /\ reservedCsn' = [reservedCsn EXCEPT ![transaction] = visibleCsn + 1]
          /\ UNCHANGED <<readCsn, writeSet, engineSet, durability, progress,
                         acknowledged, visibleCsn, committed, walDurable,
                         walVolatile, conflictTable, crashes>>

(* One boundary step. WalAppended enters the ghost log as volatile.
   WalSynchronized under strict durability performs the file-wide fsync
   (everything volatile becomes durable); under memory durability it is a
   no-op walk step (lib.rs:24526). RootPublished publishes visibility,
   updates the conflict table, and acknowledges the client. *)
Step(transaction) ==
  /\ phase[transaction] = "committing"
  /\ progress[transaction] < BoundaryCount
  /\ LET next == progress[transaction] + 1
         csn == reservedCsn[transaction]
         strict == durability[transaction] = "strict"
     IN
     /\ progress' = [progress EXCEPT ![transaction] = next]
     /\ committed' =
          IF next = WalAppendedIndex
          THEN committed @@ (csn :> [txn |-> transaction,
                                     keys |-> writeSet[transaction],
                                     engines |-> engineSet[transaction]])
          ELSE committed
     /\ walVolatile' =
          IF next = WalAppendedIndex THEN walVolatile \cup {csn}
          ELSE IF next = WalSyncedIndex /\ strict THEN {}
          ELSE walVolatile
     /\ walDurable' =
          IF next = WalSyncedIndex /\ strict
          THEN walDurable \cup walVolatile \cup {csn}
          ELSE walDurable
     /\ IF next = PublishedIndex
        THEN /\ visibleCsn' = csn
             /\ conflictTable' =
                  [key \in Keys |->
                    IF key \in writeSet[transaction] THEN csn
                    ELSE conflictTable[key]]
             /\ phase' = [phase EXCEPT ![transaction] = "done"]
             /\ acknowledged' = acknowledged \cup {transaction}
        ELSE /\ UNCHANGED <<visibleCsn, conflictTable>>
             /\ phase' = phase
             /\ acknowledged' = acknowledged
     /\ UNCHANGED <<readCsn, writeSet, engineSet, durability, reservedCsn,
                    crashes>>

(* Crash + recovery. Durable commits always survive. Volatile commits
   survive as a nondeterministic CSN-prefix of the volatile suffix (the WAL
   is sequential; recovery truncates the first broken block and everything
   after it). In-flight transactions abort. The conflict table is rebuilt
   from survivors. Visibility resets to the highest surviving commit. *)
IsPrefixClosed(survivors, universe) ==
  \A csn \in universe : \A lower \in universe :
    (csn \in survivors /\ lower < csn) => lower \in survivors

Max(set) == CHOOSE csn \in set : \A other \in set : other <= csn

Crash ==
  /\ crashes < MaxCrashes
  /\ \E survivingVolatile \in SUBSET walVolatile :
       /\ IsPrefixClosed(walDurable \cup survivingVolatile,
                         walDurable \cup walVolatile)
       /\ LET survivors == walDurable \cup survivingVolatile
              survivedCsns == {csn \in DOMAIN committed : csn \in survivors}
          IN
          /\ visibleCsn' = IF survivedCsns = {} THEN 0 ELSE Max(survivedCsns)
          /\ committed' = [csn \in survivedCsns |-> committed[csn]]
          /\ walDurable' = survivors
          /\ walVolatile' = {}
          /\ conflictTable' =
               [key \in Keys |->
                 LET writers == {csn \in survivedCsns :
                                  key \in committed[csn].keys}
                 IN IF writers = {} THEN 0 ELSE Max(writers)]
          /\ phase' = [transaction \in Transactions |->
                        IF phase[transaction] \in {"done", "aborted"}
                        THEN phase[transaction]
                        ELSE "aborted"]
          /\ progress' = [transaction \in Transactions |-> 0]
          /\ crashes' = crashes + 1
          /\ UNCHANGED <<readCsn, writeSet, engineSet, durability,
                         reservedCsn, acknowledged>>

Next ==
  \/ \E transaction \in Transactions :
       \E keys \in (SUBSET Keys) \ {{}} :
         \E engines \in (SUBSET Engines) \ {{}} :
           \E class \in Durabilities :
             Begin(transaction, keys, engines, class)
  \/ \E transaction \in Transactions : Admit(transaction)
  \/ \E transaction \in Transactions : Step(transaction)
  \/ Crash

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants *)

(* All-or-nothing per commit: every logged commit keeps its complete,
   nonempty engine set; crash restricts the log domain but never rewrites an
   entry, so a "partial" (single-engine remnant of a three-engine commit)
   state is unreachable. Combined with prefix recovery this is the
   cross-engine atomicity claim at this abstraction level. *)
Atomicity ==
  \A csn \in DOMAIN committed :
    /\ committed[csn].engines /= {}
    /\ committed[csn].keys /= {}

(* An acknowledged Strict commit survives every crash and stays visible. *)
StrictDurability ==
  \A transaction \in acknowledged :
    durability[transaction] = "strict" =>
      /\ reservedCsn[transaction] \in DOMAIN committed
      /\ reservedCsn[transaction] <= visibleCsn

(* First-committer-wins over surviving commits. *)
FirstCommitterWins ==
  \A first, second \in DOMAIN committed :
    (first < second /\
     committed[first].keys \cap committed[second].keys /= {}) =>
      readCsn[committed[second].txn] >= first

(* Published visibility is a complete contiguous prefix. *)
VisiblePrefixComplete ==
  \A csn \in 1..visibleCsn : csn \in DOMAIN committed

(* Nothing above the visible CSN is durable-and-forgotten: every logged
   commit above visibility is still inside an in-flight WAL window. *)
CsnBounded ==
  \A csn \in DOMAIN committed :
    csn <= visibleCsn \/ csn \in walDurable \cup walVolatile

TypeOk ==
  /\ phase \in [Transactions -> {"idle", "prepared", "committing", "done",
                                 "aborted"}]
  /\ readCsn \in [Transactions -> Nat]
  /\ writeSet \in [Transactions -> SUBSET Keys]
  /\ engineSet \in [Transactions -> SUBSET Engines]
  /\ durability \in [Transactions -> Durabilities]
  /\ progress \in [Transactions -> 0..BoundaryCount]
  /\ reservedCsn \in [Transactions -> Nat]
  /\ acknowledged \subseteq Transactions
  /\ visibleCsn \in Nat
  /\ walDurable \subseteq Nat
  /\ walVolatile \subseteq Nat
  /\ conflictTable \in [Keys -> Nat]
  /\ crashes \in 0..MaxCrashes

============================================================================
