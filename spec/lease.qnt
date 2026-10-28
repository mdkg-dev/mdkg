// -*- mode: Bluespec; -*-
/**
 * Lease/claim protocol of the mdkg kernel: leases, and the coordination
 * backends that hold them.
 *
 * One coordinator holds the authoritative lease table and the current
 * snapshot hash of every node. Agents talk to it while online:
 *
 *   claim -> heartbeat* -> commit (compare-and-swap on base hash) | release
 *
 * The holder's own work can also reach the node outside a commit (it
 * committed on main itself, or its branch merged): `land`. Its commit then
 * passes although the base is stale, because the node already holds its
 * own version and nothing else was lost (the CLI's `landed` commit).
 *
 * A confirmed lease whose deadline passes without a heartbeat is expired by
 * the coordinator. While an agent is offline it may take *provisional*
 * leases; on reconnect each one is reconciled: confirmed if it is still
 * safe, otherwise marked `Conflict` (surfaced, never silently merged).
 *
 * Abstractions:
 *   - A snapshot hash is modelled as a per-node version counter; a commit
 *     produces base + 1. Content hashes behave the same for CAS purposes.
 *   - Lease ids come from one counter; offline ids would really be
 *     hash-style aliases, but only uniqueness matters.
 *   - A lease covers exactly one node (subject_refs = {node}).
 *   - The coordinator is a single linearizable authority (local SQLite or a
 *     remote service); its own crash/recovery is out of scope here.
 *
 * Deliberate bugs can be switched on through BUGS, so the same spec doubles
 * as the "does the checker catch it" demo. See the `mc*` modules at the end.
 */
module lease {
  // ---------------------------------------------------------------- params
  const AGENTS: Set[str]
  const NODES: Set[str]
  const TTL: int          // deadline extension granted by claim and heartbeat
  const MAX_TIME: int     // bound on the logical clock (keeps TLC finite)
  const MAX_LEASES: int   // bound on lease ids (keeps TLC finite)
  const BUGS: Set[str]    // deliberate bugs to switch on; empty = correct

  // ----------------------------------------------------------------- types
  type Status = Active | Committed | Released | Expired | Conflict
  /// Why a lease ended in Conflict; surfaced to the agent with the event.
  type Reason = NoReason | Held | Stale | CasFailed

  type Lease = {
    owner: str,
    node: str,
    base: int,         // base_snapshot_hash: node hash when the lease was taken
    deadline: int,     // the lease is dead from this tick on
    status: Status,
    confirmed: bool,   // accepted by the coordinator
    offline: bool,     // taken while the coordinator was unreachable
    committedAt: int,  // tick of the commit, -1 if none
    result: int,       // result_snapshot_hash, -1 if none
    reason: Reason,    // set when status is Conflict
    endedBy: str,      // ghost: whose request ended the lease ("" while live)
    landed: int        // the node hash the holder's own work landed as, -1 if none
  }

  pure val COORDINATOR = "coordinator"
  pure val NONE = -1

  // ----------------------------------------------------------------- state
  var now: int                  // logical clock
  var nodeHash: str -> int      // current snapshot hash of each node
  var leases: int -> Lease      // every lease ever issued, by lease id
  var online: str -> bool       // can the agent reach the coordinator?
  var nextId: int
  var seen: str -> (str -> int) // node hashes each agent last synced (used offline)

  // --------------------------------------------------------------- helpers
  def isLease(id: int): bool = leases.keys().contains(id)

  /// Confirmed, still-active leases on node `n`, as the coordinator sees them.
  def holders(n: str): Set[int] =
    leases.keys().filter(id => {
      val l = leases.get(id)
      l.node == n and l.confirmed and l.status == Active
    })

  def newLease(a: str, n: str, base: int, confirmed: bool, offline: bool, status: Status): Lease = {
    owner: a,
    node: n,
    base: base,
    deadline: now + TTL,
    status: status,
    confirmed: confirmed,
    offline: offline,
    committedAt: NONE,
    result: NONE,
    reason: if (status == Conflict) Held else NoReason,
    endedBy: if (status == Active) "" else COORDINATOR,
    landed: NONE
  }

  def ended(l: Lease, s: Status, by: str): Lease = { ...l, status: s, endedBy: by }

  // ---------------------------------------------------------------- init
  action init = all {
    now' = 0,
    nodeHash' = NODES.mapBy(n => 0),
    leases' = Map(),
    online' = AGENTS.mapBy(a => true),
    nextId' = 0,
    seen' = AGENTS.mapBy(a => NODES.mapBy(n => 0)),
  }

  // -------------------------------------------------------------- actions
  action tick = all {
    now < MAX_TIME,
    now' = now + 1,
    nodeHash' = nodeHash, leases' = leases, online' = online, seen' = seen, nextId' = nextId,
  }

  action disconnect(a: str): bool = all {
    online.get(a),
    online' = online.set(a, false),
    seen' = seen.set(a, nodeHash),  // last snapshot the agent synced
    now' = now, nodeHash' = nodeHash, leases' = leases, nextId' = nextId,
  }

  action reconnect(a: str): bool = all {
    not(online.get(a)),
    online' = online.set(a, true),
    now' = now, nodeHash' = nodeHash, leases' = leases, nextId' = nextId, seen' = seen,
  }

  /// Online claim. The coordinator orders claims: if the node is held, the
  /// loser gets a lease in status Conflict.
  action claim(a: str, n: str): bool = all {
    online.get(a),
    nextId < MAX_LEASES,
    val free = holders(n) == Set() or BUGS.contains("claim_ignores_holder")
    leases' = leases.put(nextId, newLease(a, n, nodeHash.get(n), free, false, if (free) Active else Conflict)),
    nextId' = nextId + 1,
    now' = now, nodeHash' = nodeHash, online' = online, seen' = seen,
  }

  /// Offline claim: recorded locally, unknown to the coordinator, based on
  /// the snapshot the agent had when it lost the connection.
  action claimOffline(a: str, n: str): bool = all {
    not(online.get(a)),
    nextId < MAX_LEASES,
    leases' = leases.put(nextId, newLease(a, n, seen.get(a).get(n), false, true, Active)),
    nextId' = nextId + 1,
    now' = now, nodeHash' = nodeHash, online' = online, seen' = seen,
  }

  action heartbeat(a: str, id: int): bool = all {
    online.get(a),
    isLease(id),
    val l = leases.get(id)
    all {
      l.owner == a, l.confirmed, l.status == Active, now < l.deadline,
      leases' = leases.set(id, { ...l, deadline: now + TTL }),
    },
    now' = now, nodeHash' = nodeHash, online' = online, seen' = seen, nextId' = nextId,
  }

  /// A confirmed lease is released through the coordinator; a provisional
  /// one can be dropped locally while offline.
  action release(a: str, id: int): bool = all {
    isLease(id),
    val l = leases.get(id)
    all {
      l.owner == a, l.status == Active, online.get(a) or not(l.confirmed),
      leases' = leases.set(id, l.ended(Released, a)),
    },
    now' = now, nodeHash' = nodeHash, online' = online, seen' = seen, nextId' = nextId,
  }

  /// Missed heartbeat: the coordinator expires the lease (and emits an event).
  action expire(id: int): bool = all {
    isLease(id),
    val l = leases.get(id)
    all {
      l.confirmed, l.status == Active, now >= l.deadline,
      leases' = leases.set(id, l.ended(Expired, COORDINATOR)),
    },
    now' = now, nodeHash' = nodeHash, online' = online, seen' = seen, nextId' = nextId,
  }

  /// The holder's own work reaches the node outside a commit (committed on
  /// main itself, or its branch merged): a new hash, which the lease
  /// records as its own. The lease is exclusive, so nothing else changed
  /// the node meanwhile: it holds exactly the holder's version.
  action land(a: str, id: int): bool = all {
    online.get(a),
    isLease(id),
    val l = leases.get(id)
    val newHash = nodeHash.get(l.node) + 1
    all {
      // Once per lease: what landed is its version, and the model stays
      // finite.
      l.owner == a, l.confirmed, l.status == Active, now < l.deadline, l.landed == NONE,
      leases' = leases.set(id, { ...l, landed: newHash }),
      nodeHash' = nodeHash.set(l.node, newHash),
    },
    now' = now, online' = online, seen' = seen, nextId' = nextId,
  }

  /// Commit is compare-and-swap on base_snapshot_hash. A failed CAS is
  /// surfaced as Conflict and leaves the node untouched. A stale base still
  /// passes when the node holds exactly the holder's own landed version:
  /// the commit then changes nothing.
  action commit(a: str, id: int): bool = all {
    online.get(a),
    isLease(id),
    val l = leases.get(id)
    val inTime = now < l.deadline or BUGS.contains("commit_ignores_deadline")
    val landedOk = l.landed != NONE and l.landed == nodeHash.get(l.node)
    val casOk = l.base == nodeHash.get(l.node) or landedOk or BUGS.contains("commit_without_cas")
    val newHash = if (l.base != nodeHash.get(l.node) and landedOk) nodeHash.get(l.node)
                  else nodeHash.get(l.node) + 1
    all {
      l.owner == a, l.confirmed, l.status == Active, inTime,
      if (casOk) all {
        leases' = leases.set(id, { ...l, status: Committed, committedAt: now, result: newHash, endedBy: a }),
        nodeHash' = nodeHash.set(l.node, newHash),
      } else all {
        leases' = leases.set(id, { ...l.ended(Conflict, a), reason: CasFailed }),
        nodeHash' = nodeHash,
      },
    },
    now' = now, online' = online, seen' = seen, nextId' = nextId,
  }

  /// Reconnected agent presents a provisional lease. It is confirmed only if
  /// it is still in time, nobody holds the node and the node has not moved
  /// past its base; otherwise it becomes Expired or Conflict.
  action reconcile(a: str, id: int): bool = all {
    online.get(a),
    isLease(id),
    val l = leases.get(id)
    val holderFree = holders(l.node) == Set() or BUGS.contains("reconcile_ignores_holder")
    val baseOk = l.base == nodeHash.get(l.node) or BUGS.contains("reconcile_ignores_base")
    all {
      l.owner == a, not(l.confirmed), l.status == Active,
      if (now >= l.deadline)
        leases' = leases.set(id, l.ended(Expired, COORDINATOR))
      else if (holderFree and baseOk)
        leases' = leases.set(id, { ...l, confirmed: true })
      else if (BUGS.contains("reconcile_preempts_holder") and baseOk)
        // "last writer wins": the provisional lease takes the node over
        leases' = holders(l.node).fold(leases, (ls, h) => ls.set(h, { ...ls.get(h).ended(Conflict, a), reason: Held }))
                                 .set(id, { ...l, confirmed: true })
      else
        leases' = leases.set(id, { ...l.ended(Conflict, COORDINATOR), reason: if (holderFree) Stale else Held }),
    },
    now' = now, nodeHash' = nodeHash, online' = online, seen' = seen, nextId' = nextId,
  }

  action step = {
    nondet a = AGENTS.oneOf()
    nondet n = NODES.oneOf()
    // Draw only issued ids (plus 0, so the set is never empty): ids >= nextId
    // are never leases, and random simulation would waste most steps on
    // disabled lease actions. Apalache needs a constant range, hence filter.
    nondet id = 0.to(MAX_LEASES - 1).filter(i => i < nextId or i == 0).oneOf()
    any {
      tick,
      disconnect(a),
      reconnect(a),
      claim(a, n),
      claimOffline(a, n),
      heartbeat(a, id),
      release(a, id),
      expire(id),
      land(a, id),
      commit(a, id),
      reconcile(a, id),
    }
  }

  // ------------------------------------------------------------ invariants

  /// At most one confirmed active lease per node.
  val mutualExclusion = NODES.forall(n => holders(n).size() <= 1)

  /// An expired lease never commits: every commit happened before the
  /// lease's deadline, and nothing past its deadline holds a result.
  val expiredNeverCommits = leases.keys().forall(id => {
    val l = leases.get(id)
    and {
      l.status == Committed implies l.committedAt < l.deadline,
      l.status == Expired implies l.result == NONE,
    }
  })

  /// A provisional lease never silently overwrites a confirmed one:
  ///   1. only confirmed leases commit;
  ///   2. no two commits on a node start from the same snapshot (no lost update);
  ///   3. a confirmed lease is ended only by its owner or by expiry, never by
  ///      somebody else's (e.g. a reconciling agent's) request.
  val noSilentOverwrite = leases.keys().forall(i => {
    val li = leases.get(i)
    and {
      li.status == Committed implies li.confirmed,
      leases.keys().forall(j => {
        val lj = leases.get(j)
        (i != j and li.status == Committed and lj.status == Committed and li.node == lj.node)
          implies li.base != lj.base
      }),
      (li.confirmed and li.status != Active)
        implies (li.endedBy == li.owner or li.status == Expired),
    }
  })

  val safety = mutualExclusion and expiredNeverCommits and noSilentOverwrite

  /// Not a safety property: in the correct protocol the lease alone already
  /// serialises writers, so CAS never fires. It is the backstop that turns
  /// a reconcile bug into a surfaced Conflict instead of a lost update.
  val casNeverFails = leases.keys().forall(id => {
    val l = leases.get(id)
    not(l.confirmed and l.status == Conflict and l.endedBy == l.owner)
  })

  // Witnesses: each must be VIOLATED, proving the model reaches that state
  // and the invariants above are not vacuous.
  val noCommit = leases.keys().forall(id => leases.get(id).status != Committed)
  val noReconciledCommit = leases.keys().forall(id => {
    val l = leases.get(id)
    not(l.offline and l.status == Committed)
  })
  val noReconcileConflict = leases.keys().forall(id => {
    val l = leases.get(id)
    not(l.offline and l.status == Conflict)
  })
  // A provisional lease refused only because its base went stale. Random
  // simulation rarely gets here; check.sh turns it into a directed trace.
  val noStaleReconcileConflict = leases.keys().forall(id => leases.get(id).reason != Stale)
  val noExpiry = leases.keys().forall(id => leases.get(id).status != Expired)
  val noClaimConflict = leases.keys().forall(id => {
    val l = leases.get(id)
    not(not(l.offline) and l.status == Conflict)
  })
}

// ------------------------------------------------------------------------
// Model-checking instances. Small on purpose: 2 agents, 1 node, 4 ticks,
// 3 lease ids already covers claim races, expiry and offline reconcile.

module mc {
  import lease(AGENTS = Set("a1", "a2"), NODES = Set("n1"), TTL = 2, MAX_TIME = 4,
               MAX_LEASES = 3, BUGS = Set()).*
}

module mc_bug_reconcile {
  import lease(AGENTS = Set("a1", "a2"), NODES = Set("n1"), TTL = 2, MAX_TIME = 4,
               MAX_LEASES = 3, BUGS = Set("reconcile_ignores_holder")).*
}

module mc_bug_preempt {
  import lease(AGENTS = Set("a1", "a2"), NODES = Set("n1"), TTL = 2, MAX_TIME = 4,
               MAX_LEASES = 3, BUGS = Set("reconcile_preempts_holder")).*
}

module mc_bug_deadline {
  import lease(AGENTS = Set("a1", "a2"), NODES = Set("n1"), TTL = 2, MAX_TIME = 4,
               MAX_LEASES = 3, BUGS = Set("commit_ignores_deadline")).*
}

module mc_bug_lost_update {
  import lease(AGENTS = Set("a1", "a2"), NODES = Set("n1"), TTL = 2, MAX_TIME = 4,
               MAX_LEASES = 3, BUGS = Set("reconcile_ignores_base", "commit_without_cas")).*
}

// Only the reconcile base check is broken. No safety invariant fails: the
// commit-time CAS turns the stale lease into a surfaced Conflict. Only
// `casNeverFails` (informational) is violated.
module mc_bug_base {
  import lease(AGENTS = Set("a1", "a2"), NODES = Set("n1"), TTL = 2, MAX_TIME = 4,
               MAX_LEASES = 3, BUGS = Set("reconcile_ignores_base")).*
}

// Larger instance for random simulation and for generating the traces that
// drive the Rust model-based test (too big for exhaustive TLC).
module sim {
  import lease(AGENTS = Set("a1", "a2", "a3"), NODES = Set("n1", "n2"), TTL = 3,
               MAX_TIME = 15, MAX_LEASES = 10, BUGS = Set()).*
}
