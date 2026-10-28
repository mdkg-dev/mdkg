# Scoped configuration edits

`Prepared::new` chooses exactly one project, user or machine-local file from
captured settings paths. Preparation reads and builds the candidate without
creating directories or writing files. User settings can be prepared outside
a graph; machine-local settings need a discovered Git common directory.

Keys use dotted object paths or RFC 6901 JSON pointers. Arrays are replaced
whole. `settings::edit::value` accepts JSON values and bare-word strings;
JSON-looking malformed input is refused. A numeric-looking string can be
quoted as JSON. Unknown keys survive edits. Identity keys are managed by init
and migration. An explicit storage-choice edit writes `work.location` and
removes its old alias; unrelated edits retain that alias.

The caller validates the complete candidate before any write, using the
relevant graph, launcher and agent validators. For graph fields, use
`Project::resolve_replacing` and `Resolved::config`. The replacement is
checked against the captured source path and cannot change project identity.
The command layer is responsible for graph-repair and project-lease checks.
CLI dispatch is connected separately.

Apply checks the captured file's bytes, inode and rwx mode, and the identities
of its checked folders. Writes and renames use those directory handles.
Final file links, special files and new links below the anchor are refused.
No-op edits retain their original bytes and inode. New files are private;
replacements preserve rwx permissions and leave other hard links unchanged.

Cooperating setters hold the old file's advisory lock and lock the staged
replacement before publication. Creating an absent file uses a no-replace
rename. Editors that ignore locks are outside this serialization boundary;
changes observed before publication are refused. This is local coordination,
not authentication against another process running as the same user.

The staged file is synced before rename and its folder is synced afterward.
`Failure.published` is true if publication succeeded but durability could not
be confirmed, and the error text says so. Before publication, an error keeps
the target untouched by the operation. A failed process can leave a `.mdkg-config-*.tmp`;
retrying does not remove another operation's staging files. The operations do
not open a coordinator store or run Git.

The filesystem tests cover scopes, preview refusal, stale files, directory
swaps, concurrent setters, permission/hard-link handling and failure states.
The CLI's `config_operations` tests exercise the complete library pipeline
with temporary homes and handwritten repository metadata.
