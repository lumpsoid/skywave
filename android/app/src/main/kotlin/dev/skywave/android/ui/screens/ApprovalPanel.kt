package dev.skywave.android.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.Notifications
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.Button
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.skywave.android.R
import dev.skywave.android.ui.toCount
import dev.skywave.core.Answer
import dev.skywave.core.ApprovalPrompt
import dev.skywave.core.ApprovalStatus
import dev.skywave.core.Decision
import dev.skywave.core.Outgoing
import dev.skywave.core.RuleScope
import dev.skywave.core.RuleSuggestion
import dev.skywave.core.ToolKind
import dev.skywave.core.ToolView

/** Lines of an approval's body past which it opens bounded, with "show more". */
private const val PANEL_LINES = 8

internal object ApprovalTags {
    const val PANEL = "approval_panel"
    const val STRIP = "approval_strip"
    const val OPEN = "approval_open"
    const val COLLAPSE = "approval_collapse"
    const val BODY = "approval_body"
    const val MORE = "approval_more"
    const val APPROVE = "approval_approve"
    const val DENY = "approval_deny"
    const val STATE = "approval_state"
    const val SETTLED = "approval_settled"

    /** The gated timeline item's way to the panel. */
    const val ANSWER = "approval_answer"

    /** The session toolbar's jump to the next approval elsewhere. */
    const val NEXT = "approval_next"
    fun rule(index: Int) = "approval_rule:$index"
}

/** The approval the session waits on, as the timeline item it gates sees it. */
internal class GatedApproval(val prompt: ApprovalPrompt, val open: () -> Unit)

/**
 * The approval the agent is blocked on, above the composer: what the call
 * will do (the diff, the file's content or the whole command), approve and
 * deny, and the lasting rules the agent offers. Answers are commands like
 * any other: queued offline, the controls off while one stands. Collapses
 * to a strip so the timeline can be read; the gated item reopens it.
 *
 * [tool] is the call the approval gates, null for one with no logged call.
 * Whether an answer is still taken is the core's call (D2); the panel only
 * stops offering one the replica shows settled.
 */
@Composable
internal fun ApprovalPanel(
    prompt: ApprovalPrompt,
    tool: ToolView?,
    open: Boolean,
    onOpen: (Boolean) -> Unit,
    onAnswer: (Decision, UByte?) -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    if (!open) {
        Surface(color = colors.tertiaryContainer, modifier = Modifier.fillMaxWidth().testTag(ApprovalTags.STRIP)) {
            Row(Modifier.padding(start = 16.dp, end = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    stringResource(R.string.ap_strip, tool?.let { kindLabel(it) } ?: prompt.tool, tool?.title ?: prompt.summary),
                    color = colors.onTertiaryContainer,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                TextButton(onClick = { onOpen(true) }, modifier = Modifier.testTag(ApprovalTags.OPEN)) {
                    Text(stringResource(if (answered(prompt.answer)) R.string.ap_show else R.string.ap_answer))
                }
            }
        }
        return
    }

    val body = approvalBody(prompt, tool)
    // Settled in the replica while the snapshot still names it: another
    // device answered, or the agent gave up. Nothing to offer then.
    val settled = tool?.approval?.status?.takeUnless { it is ApprovalStatus.Pending }
    val answered = answered(prompt.answer)
    var tall by rememberSaveable(prompt.id) { mutableStateOf(false) }

    Surface(color = colors.tertiaryContainer, modifier = Modifier.fillMaxWidth().testTag(ApprovalTags.PANEL)) {
        Column(
            Modifier.padding(start = 16.dp, end = 8.dp, top = 4.dp, bottom = 8.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    stringResource(R.string.ap_title),
                    style = MaterialTheme.typography.titleSmall,
                    color = colors.onTertiaryContainer,
                    modifier = Modifier.weight(1f),
                )
                IconButton(onClick = { onOpen(false) }, modifier = Modifier.testTag(ApprovalTags.COLLAPSE)) {
                    Icon(Icons.Filled.KeyboardArrowDown, stringResource(R.string.ap_collapse))
                }
            }
            Column(Modifier.padding(end = 8.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Heading(prompt, tool, body)
                if (body != ApprovalBody.None) {
                    Box(
                        Modifier
                            .fillMaxWidth()
                            .heightIn(max = if (tall) 440.dp else 180.dp)
                            .verticalScroll(rememberScrollState())
                            .testTag(ApprovalTags.BODY),
                    ) {
                        when (body) {
                            is ApprovalBody.Diff -> DiffBox(body.text)
                            is ApprovalBody.Content -> CodeBox(body.text)
                            is ApprovalBody.Command -> CodeBox(body.text, wrap = true)
                            ApprovalBody.None -> {}
                        }
                    }
                }
                if (isLong(body)) {
                    TextButton(onClick = { tall = !tall }, modifier = Modifier.testTag(ApprovalTags.MORE)) {
                        Text(stringResource(if (tall) R.string.ap_show_less else R.string.ap_show_more))
                    }
                }
                val full = (body as? ApprovalBody.Diff)?.full ?: (body as? ApprovalBody.Content)?.full
                full?.let { Minor(stringResource(R.string.sv_tool_full, blobText(it))) }
                    ?: prompt.input?.let { Minor(stringResource(R.string.sv_tool_input, blobText(it))) }

                if (settled != null) {
                    Text(
                        approvalText(settled),
                        color = colors.onTertiaryContainer,
                        fontWeight = FontWeight.SemiBold,
                        modifier = Modifier.testTag(ApprovalTags.SETTLED),
                    )
                    return@Column
                }
                if (!answered) {
                    prompt.suggestions.forEachIndexed { index, rule ->
                        OutlinedButton(
                            onClick = { onAnswer(Decision.APPROVE, index.toUByte()) },
                            modifier = Modifier.fillMaxWidth().testTag(ApprovalTags.rule(index)),
                        ) {
                            Text(stringResource(R.string.ap_always_allow, ruleText(rule)))
                        }
                    }
                }
                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End),
                ) {
                    OutlinedButton(
                        onClick = { onAnswer(Decision.DENY, null) },
                        enabled = !answered,
                        modifier = Modifier.testTag(ApprovalTags.DENY),
                    ) { Text(stringResource(R.string.ap_deny)) }
                    Button(
                        onClick = { onAnswer(Decision.APPROVE, null) },
                        enabled = !answered,
                        modifier = Modifier.testTag(ApprovalTags.APPROVE),
                    ) { Text(stringResource(R.string.ap_approve)) }
                }
                prompt.answer?.let { answer ->
                    Text(
                        answerText(answer),
                        style = MaterialTheme.typography.bodySmall,
                        color = if (answer.outgoing is Outgoing.Failed) colors.error else colors.onTertiaryContainer,
                        modifier = Modifier.testTag(ApprovalTags.STATE),
                    )
                }
            }
        }
    }
}

/** Kind and title of the call, as the timeline shows it, and its detail. */
@Composable
private fun Heading(prompt: ApprovalPrompt, tool: ToolView?, body: ApprovalBody) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(
            tool?.let { kindLabel(it) } ?: prompt.tool,
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.primary,
            fontWeight = FontWeight.SemiBold,
        )
        // A command is the body's to show whole; the title would repeat it.
        if (body !is ApprovalBody.Command) {
            val title = tool?.title ?: prompt.summary.lineSequence().first()
            Text(
                title,
                fontFamily = if (tool?.kind == ToolKind.SHELL) FontFamily.Monospace else FontFamily.Default,
                color = MaterialTheme.colorScheme.onTertiaryContainer,
                maxLines = 3,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
    tool?.detail?.let { Minor(it) }
}

private fun isLong(body: ApprovalBody): Boolean = when (body) {
    is ApprovalBody.Diff -> body.lines > PANEL_LINES.toUInt()
    is ApprovalBody.Content -> body.lines > PANEL_LINES.toUInt()
    is ApprovalBody.Command -> body.text.lines().size > PANEL_LINES
    ApprovalBody.None -> false
}

/**
 * How the gated item states its approval: pending ones the session waits on
 * carry a way to the panel, until this device has answered. A settled one
 * also states this device's [answer] if it did not apply.
 */
@Composable
internal fun ApprovalLine(status: ApprovalStatus, gated: GatedApproval?, answer: Answer? = null) {
    val prompt = gated?.prompt
    if (status !is ApprovalStatus.Pending || prompt == null) {
        Minor(approvalText(status))
        answer?.takeIf { status !is ApprovalStatus.Pending && it.outgoing is Outgoing.Failed }
            ?.let { Minor(answerText(it), color = MaterialTheme.colorScheme.error) }
        return
    }
    val given = prompt.answer
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            if (given != null) answerText(given) else stringResource(R.string.ap_waiting_you),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.tertiary,
            fontWeight = FontWeight.SemiBold,
            modifier = Modifier.weight(1f),
        )
        if (!answered(given)) {
            TextButton(onClick = gated.open, modifier = Modifier.testTag(ApprovalTags.ANSWER)) {
                Text(stringResource(R.string.ap_answer))
            }
        }
    }
}

/** "You approved · queued, goes out when back online". */
@Composable
internal fun answerText(answer: Answer): String {
    val said = when {
        answer.rule != null -> stringResource(R.string.ap_you_approved_rule, ruleText(answer.rule!!))
        answer.decision == Decision.APPROVE -> stringResource(R.string.ap_you_approved)
        else -> stringResource(R.string.ap_you_denied)
    }
    val delivery = when (val out = answer.outgoing) {
        Outgoing.Queued -> stringResource(R.string.ap_out_queued)
        Outgoing.Sending -> stringResource(R.string.ap_out_sending)
        is Outgoing.Accepted -> stringResource(R.string.ap_out_accepted)
        // Answering again only helps if the approval still takes one.
        is Outgoing.Failed -> stringResource(
            if (answer.retryable) R.string.ap_out_failed else R.string.ap_out_unapplied,
            failText(out.reason),
        )
    }
    return said + stringResource(R.string.separator) + delivery
}

/** A rule as offered: its label, then how far it reaches. */
@Composable
internal fun ruleText(rule: RuleSuggestion): String = stringResource(
    R.string.ap_rule,
    rule.label,
    stringResource(
        when (rule.scope) {
            RuleScope.SESSION -> R.string.ap_scope_session
            RuleScope.PROJECT -> R.string.ap_scope_project
            RuleScope.USER -> R.string.ap_scope_user
        },
    ),
)

/**
 * The toolbar's jump to the next approval waiting in another session, with
 * how many wait; nothing when none do.
 */
@Composable
internal fun NextApprovalButton(elsewhere: ULong, onClick: () -> Unit) {
    if (elsewhere == 0uL) return
    val n = elsewhere.toCount()
    val described = pluralStringResource(R.plurals.ap_next, n, n)
    IconButton(
        onClick = onClick,
        modifier = Modifier.testTag(ApprovalTags.NEXT).semantics { contentDescription = described },
    ) {
        BadgedBox(badge = { Badge { Text(n.toString()) } }) {
            Icon(Icons.Filled.Notifications, null)
        }
    }
}
