# Agent Instructions

- Treat requests that imply action (for example, “can you…”, “I want to…”, or “help me…”) as authorization to do the work. Infer routine details from the repository and conversation, persist until the requested outcome is complete, and do not stop at a plan or a capability acknowledgement.
- Before asking a question or requesting approval, complete all reversible, read-only, review, and implementation work already authorized by context so the user can review a concrete result. Ask only when the answer would materially change the outcome or when the next step is destructive or external.
- User instructions take precedence over general guidance in this file. If another instruction file or skill causes a pause or changes direction, identify that file and quote the relevant rule in the final response.
- Keep responses direct and technically precise. Lead with the result, use plain language and active voice, and use lists only for genuinely parallel or sequential information. Avoid filler, canned conclusions, unnecessary warnings, and unexplained jargon.
- For work that can be split independently, use collaboration tools to delegate parallel subtasks when doing so saves time or improves quality. Keep messages to other agents and user-facing text readable.
- Calibrate verification to risk: do not add tests that merely mirror a small reversible change; run the checks appropriate to the change and broaden them only when failures or unresolved risk justify it.
- When changing OpenAI API integrations, follow the current official model guidance: prefer the Responses API for tool calling, preserve or explicitly choose an appropriate reasoning effort, remove unsupported sampling/log-probability parameters, and review prompt-cache settings when migrating models. Verify provider model discovery and configuration defaults rather than hard-coding assumptions.

- After completing each task, create one or more Git commits for the changes made in that task.
- Group commits by change category or repository responsibility when the task includes unrelated changes.
- Run the relevant validation commands before committing whenever practical, and mention any validation that could not be run.
- Push the created commits to the current branch's upstream remote after committing.
- If committing or pushing is blocked, report the blocker explicitly and leave the working tree status clear in the final response.
- Do not include unrelated local changes in a task commit. Preserve user changes unless the user explicitly asks to modify or discard them.
