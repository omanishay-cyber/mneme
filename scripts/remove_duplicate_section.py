import os
filepath = os.path.expanduser('~/crg/datatree/docs/design/2026-04-23-datatree-design.md')

with open(filepath, 'r', encoding='utf-8') as f:
    content = f.read()

# The duplicate section 13.5 starts after section 19 ends (after the --- separator following line 1510)
# We need to find the second occurrence of "## 13.5" and remove from there to "## 16.5" (next top-level section)
# The section to keep is the first occurrence (the one we inserted before section 14)
# The section to remove is the second occurrence (the original that was after section 19)

marker_second = '\n\n## 13.5 Database Operations Layer (Builder / Finder / AccessPath / Query / Response / Injection / Lifecycle)'

# Find where the second instance of section 13.5 starts
idx = content.find(marker_second)
if idx == -1:
    print("Second section 13.5 marker not found - checking alternate form")
    # try without leading newlines
    marker_second = '## 13.5 Database Operations Layer (Builder / Finder / AccessPath / Query / Response / Injection / Lifecycle)'
    idx = content.find(marker_second)
    if idx == -1:
        print("ERROR: Could not find second section 13.5")
        exit(1)

print(f"Found second section 13.5 at character index {idx}")
print(f"Context: ...{repr(content[idx-50:idx+100])}...")

# Find the next top-level section after the duplicate (## 16.5 Subagent Roster)
next_section_marker = '\n\n## 16.5 Subagent Roster'
next_idx = content.find(next_section_marker, idx)
if next_idx == -1:
    print("ERROR: Could not find ## 16.5 after the duplicate section")
    exit(1)

print(f"Next section (16.5) at character index {next_idx}")

# Remove the duplicate section: from idx to next_idx (keep the next section)
# We need to preserve the '---' separator before the duplicate and the content after
# Let's check what's before idx
before_dup = content[idx-5:idx]
print(f"Content just before duplicate: {repr(before_dup)}")

# Remove the block from the duplicate 13.5 header up to (but not including) ## 16.5
new_content = content[:idx] + content[next_idx:]

with open(filepath, 'w', encoding='utf-8') as f:
    f.write(new_content)

print(f"SUCCESS: Removed duplicate section. New file length: {len(new_content)} chars")

# Verify
with open(filepath, 'r', encoding='utf-8') as f:
    verify = f.read()

count_13_5 = verify.count('## 13.5')
print(f"Number of '## 13.5' occurrences in file: {count_13_5}")
