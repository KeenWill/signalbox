import Foundation
#if canImport(SignalboxModels)
import SignalboxModels
#endif

public enum SignalboxWebSocketMessage: Equatable, Sendable {
    case data(Data)
    case string(String)
}

public protocol SignalboxWebSocketTransport: Sendable {
    func receive() async throws -> SignalboxWebSocketMessage
    func send(_ message: SignalboxWebSocketMessage) async throws
    func cancel() async
}

public struct URLSessionSignalboxWebSocketTransport: SignalboxWebSocketTransport, Sendable {
    private let driver: URLSessionSignalboxWebSocketDriver

    public init(url: URL, session: URLSession = .shared) {
        self.driver = URLSessionSignalboxWebSocketDriver(
            task: session.webSocketTask(with: url)
        )
    }

    public func receive() async throws -> SignalboxWebSocketMessage {
        try await driver.receive()
    }

    public func send(_ message: SignalboxWebSocketMessage) async throws {
        try await driver.send(message)
    }

    public func cancel() async {
        await driver.cancel()
    }
}

private actor URLSessionSignalboxWebSocketDriver {
    private let task: URLSessionWebSocketTask
    private var started = false

    init(task: URLSessionWebSocketTask) {
        self.task = task
    }

    func receive() async throws -> SignalboxWebSocketMessage {
        startIfNeeded()
        switch try await task.receive() {
        case .data(let data):
            return .data(data)
        case .string(let string):
            return .string(string)
        @unknown default:
            throw SignalboxWebSocketStreamError.unsupportedMessage
        }
    }

    func send(_ message: SignalboxWebSocketMessage) async throws {
        startIfNeeded()
        switch message {
        case .data(let data):
            try await task.send(.data(data))
        case .string(let string):
            try await task.send(.string(string))
        }
    }

    func cancel() {
        task.cancel(with: .goingAway, reason: nil)
    }

    private func startIfNeeded() {
        guard !started else {
            return
        }
        started = true
        task.resume()
    }
}

public enum SignalboxWebSocketStreamError: LocalizedError, Equatable {
    case connectionTimedOut
    case connectionWentQuiet
    case unsupportedMessage

    public var errorDescription: String? {
        switch self {
        case .connectionTimedOut:
            return "The server connection did not receive a heartbeat in time."
        case .connectionWentQuiet:
            return "The server connection stopped receiving heartbeats."
        case .unsupportedMessage:
            return "The server sent an unsupported WebSocket message."
        }
    }
}

public final class SignalboxWebSocketStream: Sendable {
    public static let defaultHeartbeatTimeout: Duration = .seconds(45)

    private let transportFactory: @Sendable () -> any SignalboxWebSocketTransport
    private let heartbeatTimeout: Duration

    public convenience init(
        url: URL,
        heartbeatTimeout: Duration = SignalboxWebSocketStream.defaultHeartbeatTimeout
    ) {
        self.init(
            transportFactory: {
                URLSessionSignalboxWebSocketTransport(url: url)
            },
            heartbeatTimeout: heartbeatTimeout
        )
    }

    public init(
        transportFactory: @escaping @Sendable () -> any SignalboxWebSocketTransport,
        heartbeatTimeout: Duration = SignalboxWebSocketStream.defaultHeartbeatTimeout
    ) {
        self.transportFactory = transportFactory
        self.heartbeatTimeout = heartbeatTimeout
    }

    public func messages() -> AsyncThrowingStream<SignalboxServerMessage, Error> {
        AsyncThrowingStream { continuation in
            let transport = transportFactory()
            let watchdog = SignalboxHeartbeatWatchdog(timeout: heartbeatTimeout)
            let timeoutAction: @Sendable (SignalboxWebSocketStreamError) async -> Void = { error in
                continuation.finish(throwing: error)
                await transport.cancel()
            }
            let receiveTask = Task {
                await watchdog.arm {
                    await timeoutAction(.connectionTimedOut)
                }
                do {
                    while !Task.isCancelled {
                        let message = try await transport.receive()
                        let data: Data
                        switch message {
                        case .data(let receivedData):
                            data = receivedData
                        case .string(let string):
                            data = Data(string.utf8)
                        }
                        let decoded: SignalboxServerMessage
                        do {
                            decoded = try SignalboxJSONCoding.decoder().decode(SignalboxServerMessage.self, from: data)
                        } catch {
                            continuation.yield(.diagnostic(SignalboxDecodingDiagnostic(error: error)))
                            continue
                        }
                        if case .heartbeat(let sentAt) = decoded {
                            try await sendHeartbeatAck(sentAt: sentAt, transport: transport)
                            await watchdog.arm {
                                await timeoutAction(.connectionWentQuiet)
                            }
                            continue
                        }
                        continuation.yield(decoded)
                    }
                } catch {
                    await watchdog.cancel()
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                receiveTask.cancel()
                Task {
                    await watchdog.cancel()
                    await transport.cancel()
                }
            }
        }
    }

    private func sendHeartbeatAck(
        sentAt: Date,
        transport: any SignalboxWebSocketTransport
    ) async throws {
        let payload = SignalboxHeartbeatAck(kind: "heartbeat_ack", sentAt: sentAt)
        let data = try SignalboxJSONCoding.encoder().encode(payload)
        guard let string = String(data: data, encoding: .utf8) else {
            return
        }
        try await transport.send(.string(string))
    }
}

private actor SignalboxHeartbeatWatchdog {
    private let timeout: Duration
    private var timeoutTask: Task<Void, Never>?

    init(timeout: Duration) {
        self.timeout = timeout
    }

    func arm(onTimeout: @escaping @Sendable () async -> Void) {
        timeoutTask?.cancel()
        timeoutTask = Task {
            do {
                try await Task.sleep(for: timeout)
            } catch {
                return
            }
            await onTimeout()
        }
    }

    func cancel() {
        timeoutTask?.cancel()
        timeoutTask = nil
    }
}

private struct SignalboxHeartbeatAck: Encodable {
    let kind: String
    let sentAt: Date

    private enum CodingKeys: String, CodingKey {
        case kind
        case sentAt = "sent_at"
    }
}
