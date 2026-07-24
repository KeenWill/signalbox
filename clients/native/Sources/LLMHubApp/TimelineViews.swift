#if canImport(LLMHubModels)
import LLMHubModels
#endif
import Foundation
import SwiftUI

struct MessageBubble: View {
    let message: HubTimelineMessage

    var body: some View {
        HStack {
            if message.role == .user {
                Spacer(minLength: 48)
            }
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 6) {
                    Label(roleLabel, systemImage: roleIcon)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    if message.isStreaming {
                        ProgressView()
                            .controlSize(.small)
                    }
                }
                if let thinkingText = message.thinkingText {
                    DisclosureGroup {
                        Text(thinkingText)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    } label: {
                        Text("Reasoning summary")
                            .font(.caption.weight(.semibold))
                    }
                }
                if !message.text.isEmpty {
                    MarkdownMessageText(markdown: message.text)
                        .font(.body)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(12)
            .frame(maxWidth: maxBubbleWidth, alignment: .leading)
            .background(backgroundStyle, in: RoundedRectangle(cornerRadius: 8))
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .strokeBorder(.primary.opacity(0.08))
            )
            if message.role != .user {
                Spacer(minLength: 32)
            }
        }
    }

    private var maxBubbleWidth: CGFloat {
        #if os(macOS)
        return message.role == .user ? 420 : 640
        #else
        return message.role == .user ? 620 : 760
        #endif
    }

    private var roleLabel: String {
        switch message.role {
        case .user:
            return "You"
        case .assistant:
            return "Assistant"
        case .system:
            return "System"
        case .tool:
            return "Tool"
        case .unknown:
            return "Message"
        }
    }

    private var roleIcon: String {
        switch message.role {
        case .user:
            return "person.crop.circle"
        case .assistant:
            return "sparkles"
        case .system:
            return "gearshape"
        case .tool:
            return "wrench.and.screwdriver"
        case .unknown:
            return "questionmark.bubble"
        }
    }

    private var backgroundStyle: Color {
        message.role == .user ? Color.accentColor.opacity(0.12) : Color.secondary.opacity(0.08)
    }
}

private struct MarkdownMessageText: View {
    let markdown: String

    var body: some View {
        let blocks = MarkdownBlockParser.parse(markdown)
        VStack(alignment: .leading, spacing: 9) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                MarkdownBlockView(block: block)
            }
        }
    }
}

private struct MarkdownBlock {
    let kind: Kind

    enum Kind {
        case heading(level: Int, text: String)
        case paragraph(String)
        case unorderedList([String])
        case orderedList([String])
        case taskList([TaskItem])
        case quote(String)
        case codeBlock(language: String?, code: String)
        case table(headers: [String], rows: [[String]])
        case divider
    }
}

private struct TaskItem {
    let isComplete: Bool
    let text: String
}

private struct MarkdownBlockView: View {
    let block: MarkdownBlock

    var body: some View {
        switch block.kind {
        case .heading(let level, let text):
            InlineMarkdownText(markdown: text)
                .font(headingFont(level: level))
                .padding(.top, level == 1 ? 2 : 0)
        case .paragraph(let text):
            InlineMarkdownText(markdown: text)
        case .unorderedList(let items):
            VStack(alignment: .leading, spacing: 5) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("•")
                            .foregroundStyle(.secondary)
                        InlineMarkdownText(markdown: item)
                    }
                }
            }
        case .orderedList(let items):
            VStack(alignment: .leading, spacing: 5) {
                ForEach(Array(items.enumerated()), id: \.offset) { index, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("\(index + 1).")
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                        InlineMarkdownText(markdown: item)
                    }
                }
            }
        case .taskList(let items):
            VStack(alignment: .leading, spacing: 5) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Image(systemName: item.isComplete ? "checkmark.square.fill" : "square")
                            .foregroundStyle(item.isComplete ? .green : .secondary)
                        InlineMarkdownText(markdown: item.text)
                    }
                }
            }
        case .quote(let text):
            HStack(alignment: .top, spacing: 10) {
                RoundedRectangle(cornerRadius: 2)
                    .fill(Color.accentColor.opacity(0.45))
                    .frame(width: 4)
                InlineMarkdownText(markdown: text)
                    .foregroundStyle(.secondary)
            }
            .padding(.vertical, 4)
        case .codeBlock(let language, let code):
            VStack(alignment: .leading, spacing: 6) {
                if let language, !language.isEmpty {
                    Text(language)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                }
                ScrollView(.horizontal, showsIndicators: true) {
                    Text(code)
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .background(.black.opacity(0.07), in: RoundedRectangle(cornerRadius: 6))
            }
        case .table(let headers, let rows):
            MarkdownTable(headers: headers, rows: rows)
        case .divider:
            Divider()
        }
    }

    private func headingFont(level: Int) -> Font {
        switch level {
        case 1:
            return .title3.weight(.semibold)
        case 2:
            return .headline.weight(.semibold)
        default:
            return .subheadline.weight(.semibold)
        }
    }
}

private struct InlineMarkdownText: View {
    let markdown: String

    private var attributedMarkdown: AttributedString? {
        try? AttributedString(
            markdown: markdown,
            options: AttributedString.MarkdownParsingOptions(
                interpretedSyntax: .full,
                failurePolicy: .returnPartiallyParsedIfPossible
            )
        )
    }

    var body: some View {
        if let attributedMarkdown {
            Text(attributedMarkdown)
        } else {
            Text(markdown)
        }
    }
}

private struct MarkdownTable: View {
    let headers: [String]
    let rows: [[String]]

    var body: some View {
        ScrollView(.horizontal, showsIndicators: true) {
            Grid(alignment: .leading, horizontalSpacing: 0, verticalSpacing: 0) {
                GridRow {
                    ForEach(headers.indices, id: \.self) { index in
                        tableCell(headers[index], isHeader: true)
                    }
                }
                ForEach(rows.indices, id: \.self) { rowIndex in
                    GridRow {
                        ForEach(headers.indices, id: \.self) { columnIndex in
                            tableCell(cellText(row: rows[rowIndex], columnIndex: columnIndex), isHeader: false)
                        }
                    }
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(.primary.opacity(0.12)))
        }
    }

    private func tableCell(_ text: String, isHeader: Bool) -> some View {
        InlineMarkdownText(markdown: text)
            .font(isHeader ? .caption.weight(.semibold) : .caption)
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .frame(minWidth: 120, maxWidth: 220, alignment: .leading)
            .background(isHeader ? Color.secondary.opacity(0.14) : Color.secondary.opacity(0.05))
            .border(Color.primary.opacity(0.08), width: 0.5)
    }

    private func cellText(row: [String], columnIndex: Int) -> String {
        guard row.indices.contains(columnIndex) else {
            return ""
        }
        return row[columnIndex]
    }
}

private enum MarkdownBlockParser {
    static func parse(_ markdown: String) -> [MarkdownBlock] {
        let lines = markdown
            .replacingOccurrences(of: "\r\n", with: "\n")
            .split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init)
        var blocks: [MarkdownBlock] = []
        var paragraphLines: [String] = []
        var index = 0

        func flushParagraph() {
            guard !paragraphLines.isEmpty else {
                return
            }
            blocks.append(MarkdownBlock(kind: .paragraph(paragraphLines.joined(separator: "\n"))))
            paragraphLines.removeAll()
        }

        while index < lines.count {
            let line = lines[index]
            let trimmedLine = line.trimmingCharacters(in: .whitespaces)
            if trimmedLine.isEmpty {
                flushParagraph()
                index += 1
                continue
            }

            if let codeBlock = parseCodeBlock(lines: lines, startIndex: index) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .codeBlock(language: codeBlock.language, code: codeBlock.code)))
                index = codeBlock.nextIndex
                continue
            }

            if let heading = parseHeading(trimmedLine) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .heading(level: heading.level, text: heading.text)))
                index += 1
                continue
            }

            if isDivider(trimmedLine) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .divider))
                index += 1
                continue
            }

            if let table = parseTable(lines: lines, startIndex: index) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .table(headers: table.headers, rows: table.rows)))
                index = table.nextIndex
                continue
            }

            if let quote = parseQuote(lines: lines, startIndex: index) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .quote(quote.text)))
                index = quote.nextIndex
                continue
            }

            if let taskList = parseTaskList(lines: lines, startIndex: index) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .taskList(taskList.items)))
                index = taskList.nextIndex
                continue
            }

            if let unorderedList = parseUnorderedList(lines: lines, startIndex: index) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .unorderedList(unorderedList.items)))
                index = unorderedList.nextIndex
                continue
            }

            if let orderedList = parseOrderedList(lines: lines, startIndex: index) {
                flushParagraph()
                blocks.append(MarkdownBlock(kind: .orderedList(orderedList.items)))
                index = orderedList.nextIndex
                continue
            }

            paragraphLines.append(line)
            index += 1
        }

        flushParagraph()
        return blocks
    }

    private static func parseHeading(_ line: String) -> (level: Int, text: String)? {
        let level = line.prefix(while: { $0 == "#" }).count
        guard (1...4).contains(level), line.dropFirst(level).first == " " else {
            return nil
        }
        return (level, String(line.dropFirst(level + 1)))
    }

    private static func parseCodeBlock(lines: [String], startIndex: Int) -> (language: String?, code: String, nextIndex: Int)? {
        let openingLine = lines[startIndex].trimmingCharacters(in: .whitespaces)
        guard openingLine.hasPrefix("```") else {
            return nil
        }
        let language = String(openingLine.dropFirst(3)).trimmingCharacters(in: .whitespaces)
        var codeLines: [String] = []
        var index = startIndex + 1
        while index < lines.count {
            if lines[index].trimmingCharacters(in: .whitespaces).hasPrefix("```") {
                return (language.isEmpty ? nil : language, codeLines.joined(separator: "\n"), index + 1)
            }
            codeLines.append(lines[index])
            index += 1
        }
        return (language.isEmpty ? nil : language, codeLines.joined(separator: "\n"), index)
    }

    private static func parseTable(lines: [String], startIndex: Int) -> (headers: [String], rows: [[String]], nextIndex: Int)? {
        guard lines.indices.contains(startIndex + 1),
              isTableRow(lines[startIndex]),
              isTableSeparatorRow(lines[startIndex + 1]) else {
            return nil
        }
        let headers = splitTableRow(lines[startIndex])
        var rows: [[String]] = []
        var index = startIndex + 2
        while index < lines.count, isTableRow(lines[index]), !isTableSeparatorRow(lines[index]) {
            rows.append(splitTableRow(lines[index]))
            index += 1
        }
        return (headers, rows, index)
    }

    private static func parseQuote(lines: [String], startIndex: Int) -> (text: String, nextIndex: Int)? {
        guard lines[startIndex].trimmingCharacters(in: .whitespaces).hasPrefix(">") else {
            return nil
        }
        var quoteLines: [String] = []
        var index = startIndex
        while index < lines.count {
            let trimmedLine = lines[index].trimmingCharacters(in: .whitespaces)
            guard trimmedLine.hasPrefix(">") else {
                break
            }
            quoteLines.append(String(trimmedLine.dropFirst()).trimmingCharacters(in: .whitespaces))
            index += 1
        }
        return (quoteLines.joined(separator: "\n"), index)
    }

    private static func parseTaskList(lines: [String], startIndex: Int) -> (items: [TaskItem], nextIndex: Int)? {
        var items: [TaskItem] = []
        var index = startIndex
        while index < lines.count, let item = taskListItem(lines[index]) {
            items.append(item)
            index += 1
        }
        return items.isEmpty ? nil : (items, index)
    }

    private static func parseUnorderedList(lines: [String], startIndex: Int) -> (items: [String], nextIndex: Int)? {
        var items: [String] = []
        var index = startIndex
        while index < lines.count, let item = unorderedListItem(lines[index]) {
            items.append(item)
            index += 1
        }
        return items.isEmpty ? nil : (items, index)
    }

    private static func parseOrderedList(lines: [String], startIndex: Int) -> (items: [String], nextIndex: Int)? {
        var items: [String] = []
        var index = startIndex
        while index < lines.count, let item = orderedListItem(lines[index]) {
            items.append(item)
            index += 1
        }
        return items.isEmpty ? nil : (items, index)
    }

    private static func taskListItem(_ line: String) -> TaskItem? {
        let trimmedLine = line.trimmingCharacters(in: .whitespaces)
        if trimmedLine.hasPrefix("- [x] ") || trimmedLine.hasPrefix("- [X] ") {
            return TaskItem(isComplete: true, text: String(trimmedLine.dropFirst(6)))
        }
        if trimmedLine.hasPrefix("- [ ] ") {
            return TaskItem(isComplete: false, text: String(trimmedLine.dropFirst(6)))
        }
        return nil
    }

    private static func unorderedListItem(_ line: String) -> String? {
        let trimmedLine = line.trimmingCharacters(in: .whitespaces)
        if trimmedLine.hasPrefix("- "), !trimmedLine.hasPrefix("- [") {
            return String(trimmedLine.dropFirst(2))
        }
        if trimmedLine.hasPrefix("* ") {
            return String(trimmedLine.dropFirst(2))
        }
        return nil
    }

    private static func orderedListItem(_ line: String) -> String? {
        let trimmedLine = line.trimmingCharacters(in: .whitespaces)
        guard let separatorIndex = trimmedLine.firstIndex(of: ".") else {
            return nil
        }
        let prefix = trimmedLine[..<separatorIndex]
        guard !prefix.isEmpty, prefix.allSatisfy(\.isNumber) else {
            return nil
        }
        let afterSeparator = trimmedLine.index(after: separatorIndex)
        guard trimmedLine.indices.contains(afterSeparator), trimmedLine[afterSeparator] == " " else {
            return nil
        }
        return String(trimmedLine[trimmedLine.index(after: afterSeparator)...])
    }

    private static func isDivider(_ line: String) -> Bool {
        let characters = Array(line)
        guard characters.count >= 3 else {
            return false
        }
        return characters.allSatisfy { $0 == "-" || $0 == "*" || $0 == "_" }
    }

    private static func isTableRow(_ line: String) -> Bool {
        let trimmedLine = line.trimmingCharacters(in: .whitespaces)
        return trimmedLine.hasPrefix("|") && trimmedLine.hasSuffix("|") && trimmedLine.dropFirst().contains("|")
    }

    private static func isTableSeparatorRow(_ line: String) -> Bool {
        let cells = splitTableRow(line)
        guard !cells.isEmpty else {
            return false
        }
        return cells.allSatisfy { cell in
            let trimmedCell = cell.trimmingCharacters(in: .whitespaces)
            return trimmedCell.contains("-") && trimmedCell.allSatisfy { $0 == "-" || $0 == ":" }
        }
    }

    private static func splitTableRow(_ line: String) -> [String] {
        let trimmedLine = line.trimmingCharacters(in: .whitespaces)
        let withoutLeadingPipe = trimmedLine.hasPrefix("|") ? trimmedLine.dropFirst() : Substring(trimmedLine)
        let withoutOuterPipes = withoutLeadingPipe.hasSuffix("|") ? withoutLeadingPipe.dropLast() : withoutLeadingPipe
        return withoutOuterPipes
            .split(separator: "|", omittingEmptySubsequences: false)
            .map { $0.trimmingCharacters(in: .whitespaces) }
    }
}

struct ToolInvocationCard: View {
    let tool: HubToolCard
    let onApprove: () -> Void
    let onDeny: () -> Void
    @State private var isExpanded = true

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .center, spacing: 10) {
                Image(systemName: iconName)
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(statusColor)
                    .frame(width: 28)
                VStack(alignment: .leading, spacing: 3) {
                    Text(tool.toolName)
                        .font(.headline)
                    Text(tool.status.label)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(statusColor)
                }
                Spacer()
                Button {
                    isExpanded.toggle()
                } label: {
                    Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(isExpanded ? "Collapse tool" : "Expand tool")
            }

            if tool.status == .waitingForApproval {
                HStack(spacing: 10) {
                    Button(role: .destructive) {
                        onDeny()
                    } label: {
                        Label("Deny", systemImage: "xmark.circle")
                    }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("deny-tool-button")

                    Button {
                        onApprove()
                    } label: {
                        Label("Approve", systemImage: "checkmark.circle.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("approve-tool-button")
                }
            }

            if isExpanded {
                VStack(alignment: .leading, spacing: 8) {
                    CodeBlock(title: "Arguments", content: tool.compactArgumentSummary)
                    if !tool.statusUpdates.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Status")
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(.secondary)
                            ForEach(tool.statusUpdates, id: \.self) { update in
                                Label(update, systemImage: "smallcircle.filled.circle")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                    CodeBlock(title: "Output", content: tool.outputPreview)
                    #if os(iOS)
                    if let output = tool.output, !output.isEmpty {
                        ShareLink(item: output) {
                            Label("Share Output", systemImage: "square.and.arrow.up")
                        }
                        .font(.caption.weight(.semibold))
                    }
                    #endif
                }
            }
        }
        .padding(14)
        .background(statusColor.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(statusColor.opacity(tool.status == .waitingForApproval ? 0.7 : 0.25), lineWidth: tool.status == .waitingForApproval ? 2 : 1)
        )
        .frame(maxWidth: maxCardWidth, alignment: .leading)
    }

    private var maxCardWidth: CGFloat? {
        #if os(macOS)
        return 840
        #else
        return nil
        #endif
    }

    private var statusColor: Color {
        switch tool.status {
        case .waitingForApproval:
            return .orange
        case .failed, .denied:
            return .red
        case .succeeded:
            return .green
        case .running, .approved, .completed:
            return .blue
        }
    }

    private var iconName: String {
        switch tool.status {
        case .waitingForApproval:
            return "exclamationmark.triangle.fill"
        case .failed:
            return "xmark.octagon.fill"
        case .denied:
            return "hand.raised.fill"
        case .succeeded:
            return "checkmark.seal.fill"
        case .completed:
            return "checkmark.circle.fill"
        case .running, .approved:
            return "terminal"
        }
    }
}

private struct CodeBlock: View {
    let title: String
    let content: String

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            ScrollView(.horizontal, showsIndicators: true) {
                Text(content)
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(.black.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
        }
    }
}

struct FailureCard: View {
    let failure: HubTurnFailureCard

    var body: some View {
        Label(failure.reason, systemImage: "xmark.octagon.fill")
            .font(.callout.weight(.semibold))
            .foregroundStyle(.red)
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }
}

struct UnknownEventCard: View {
    let unknown: HubUnknownEventCard

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Label("Unknown event: \(unknown.kind)", systemImage: "questionmark.diamond")
                .font(.callout.weight(.semibold))
            Text(unknown.diagnostic.isEmpty ? "No decodable fields" : unknown.diagnostic)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }
}
