#if canImport(LLMHubModels)
import LLMHubModels
#endif
import SwiftUI

struct StatusBadge: View {
    let status: HubSessionStatus

    var body: some View {
        Label(status.label, systemImage: iconName)
            .font(.caption.weight(.semibold))
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .foregroundStyle(foregroundColor)
            .background(backgroundColor, in: Capsule())
            .accessibilityLabel(status.label)
    }

    private var iconName: String {
        switch status.state {
        case .idle:
            return "checkmark.circle"
        case .prompting:
            return "arrow.triangle.2.circlepath"
        case .waitingForConfirmation:
            return "exclamationmark.triangle.fill"
        case .failed:
            return "xmark.octagon.fill"
        case .compacting, .stopping:
            return "clock"
        case .unknown:
            return "questionmark.circle"
        }
    }

    private var foregroundColor: Color {
        switch status.state {
        case .waitingForConfirmation, .failed:
            return .white
        case .prompting:
            return .blue
        case .idle:
            return .green
        case .compacting, .stopping, .unknown:
            return .secondary
        }
    }

    private var backgroundColor: Color {
        switch status.state {
        case .waitingForConfirmation:
            return .orange
        case .failed:
            return .red
        case .prompting:
            return .blue.opacity(0.12)
        case .idle:
            return .green.opacity(0.14)
        case .compacting, .stopping, .unknown:
            return .secondary.opacity(0.12)
        }
    }
}

struct RunnerStatusBadge: View {
    let status: HubRunnerStatus

    var body: some View {
        Label(label, systemImage: status == .online ? "circle.fill" : "circle")
            .font(.caption.weight(.semibold))
            .foregroundStyle(status == .online ? .green : .secondary)
            .accessibilityIdentifier("runner-status-\(status.rawValue)")
    }

    private var label: String {
        switch status {
        case .online:
            return "Online"
        case .offline:
            return "Offline"
        case .unknown:
            return "Unknown"
        }
    }
}

struct ErrorBanner: View {
    let message: String

    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle")
            .font(.footnote)
            .foregroundStyle(.red)
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
            .accessibilityIdentifier("error-banner")
    }
}

struct SectionHeader: View {
    let title: String
    let subtitle: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.title2.weight(.bold))
            if let subtitle {
                Text(subtitle)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct EmptyStateView: View {
    let systemImage: String
    let title: String
    let message: String

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: systemImage)
                .font(.system(size: 36, weight: .semibold))
                .foregroundStyle(.secondary)
            Text(title)
                .font(.headline)
            Text(message)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding(24)
        .frame(maxWidth: .infinity, minHeight: 220)
    }
}
