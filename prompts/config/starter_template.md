# Model configuration for tinker. Uncomment and edit a slot to override
# the built-in default for that tier and backend. Commenting a line out
# reverts to the built-in default. Unrecognized model names pass through
# to the backend as-is.

[claude]
# high = "{CLAUDE_HIGH}"  # tinker, rummage, jog — built-in default
# mid  = "{CLAUDE_MID}"  # goal sessions — built-in default
# low  = "{CLAUDE_LOW}"  # cleanup — built-in default

[opencode]
# high = "{OPENCODE_HIGH}"  # tinker, rummage, jog — built-in default
# mid  = "{OPENCODE_MID}"  # goal sessions — built-in default
# low  = "{OPENCODE_LOW}"  # cleanup — built-in default

# The native backend (--native) talks to OpenRouter directly; slots are
# OpenRouter model ids. Requires OPENROUTER_API_KEY in the environment.
[native]
# high = "{NATIVE_HIGH}"  # tinker, rummage, jog — built-in default
# mid  = "{NATIVE_MID}"  # goal sessions — built-in default
# low  = "{NATIVE_LOW}"  # cleanup — built-in default
