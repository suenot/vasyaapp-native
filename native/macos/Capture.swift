// AVFoundation adapter for the Rust applications. No web runtime is involved.
import Foundation
import AVFoundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}
func authorize(_ type: AVMediaType) {
    switch AVCaptureDevice.authorizationStatus(for: type) {
    case .authorized: return
    case .notDetermined:
        let semaphore = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: type) { allowed in
            granted = allowed
            semaphore.signal()
        }
        semaphore.wait()
        if !granted { fail("Permission denied. Enable access in System Settings > Privacy & Security.") }
    default: fail("Permission denied. Enable access in System Settings > Privacy & Security.")
    }
}
final class PhotoCapture: NSObject, AVCapturePhotoCaptureDelegate {
    let session = AVCaptureSession()
    let output = AVCapturePhotoOutput()
    let destination: URL
    var finished = false
    var failure: String?
    init(destination: URL) { self.destination = destination }
    func start() throws {
        authorize(.video)
        guard let device = AVCaptureDevice.default(for: .video) else { fail("No camera available") }
        let input = try AVCaptureDeviceInput(device: device)
        session.beginConfiguration()
        session.sessionPreset = .photo
        guard session.canAddInput(input), session.canAddOutput(output) else { fail("Camera is unavailable") }
        session.addInput(input)
        session.addOutput(output)
        session.commitConfiguration()
        session.startRunning()
        output.capturePhoto(with: AVCapturePhotoSettings(format: [AVVideoCodecKey: AVVideoCodecType.jpeg]), delegate: self)
    }
    func photoOutput(_ output: AVCapturePhotoOutput, didFinishProcessingPhoto photo: AVCapturePhoto, error: Error?) {
        defer { finished = true }
        if let error { failure = error.localizedDescription; return }
        guard let data = photo.fileDataRepresentation() else { failure = "Camera returned no image"; return }
        do { try data.write(to: destination, options: .atomic) }
        catch { failure = error.localizedDescription }
    }
}
let arguments = CommandLine.arguments
if arguments.count != 3 { fail("Usage: vasya-capture record|photo output-path") }
let url = URL(fileURLWithPath: arguments[2])
do {
    switch arguments[1] {
    case "record":
        authorize(.audio)
        let recorder = try AVAudioRecorder(url: url, settings: [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: 48000, AVNumberOfChannelsKey: 1, AVEncoderAudioQualityKey: AVAudioQuality.high.rawValue])
        guard recorder.prepareToRecord(), recorder.record() else { fail("Microphone could not start recording") }
        _ = readLine()
        recorder.stop()
    case "photo":
        let capture = PhotoCapture(destination: url)
        try capture.start()
        let deadline = Date().addingTimeInterval(25)
        while !capture.finished && Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
        capture.session.stopRunning()
        if let failure = capture.failure { fail(failure) }
        if !capture.finished { fail("Camera capture timed out") }
    default: fail("Unknown capture mode")
    }
} catch { fail(error.localizedDescription) }
