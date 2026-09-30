import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";

type ArchiveFile = {
  name: string;
  relativePath: string;
  size: number;
  modifiedMs: number;
  year: number;
};

type YearSummary = {
  year: number;
  fileCount: number;
  totalSize: number;
};

type ScanResult = {
  files: ArchiveFile[];
  years: YearSummary[];
  totalFiles: number;
  totalSize: number;
  skippedFiles: number;
};

function formatBytes(bytes: number) {
  if (bytes === 0) return "0 B";

  const units = ["B", "KB", "MB", "GB", "TB"];
  const index = Math.min(
    Math.floor(Math.log(bytes) / Math.log(1024)),
    units.length - 1,
  );

  const value = bytes / 1024 ** index;
  return `${value >= 10 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}

function App() {
  const [source, setSource] = useState("");
  const [destination, setDestination] = useState("");
  const [cutoffYear, setCutoffYear] = useState("2022");
  const [result, setResult] = useState<ScanResult | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState("");
  const [showFiles, setShowFiles] = useState(false);
  const [selectedYear, setSelectedYear] = useState<number | null>(null);

  const cutoff = Number(cutoffYear);
  const canScan =
    source.length > 0 &&
    destination.length > 0 &&
    Number.isInteger(cutoff) &&
    cutoff >= 1970;

  const heading = useMemo(() => {
    if (!result) return null;
    if (result.totalFiles === 0) return "Nothing to archive";
    return "Ready to archive";
  }, [result]);

  async function chooseFolder(kind: "source" | "destination") {
    const selected = await open({
      directory: true,
      multiple: false,
      title: kind === "source" ? "Choose source folder" : "Choose archive folder",
    });

    if (typeof selected !== "string") return;

    if (kind === "source") {
      setSource(selected);
    } else {
      setDestination(selected);
    }

    setResult(null);
    setShowFiles(false);
    setSelectedYear(null);
    setError("");
  }

  async function scan() {
    if (!canScan) return;

    setScanning(true);
    setError("");
    setShowFiles(false);
    setSelectedYear(null);

    try {
      const scanResult = await invoke<ScanResult>("scan_archive", {
        source,
        archiveDestination: destination,
        cutoffYear: cutoff,
      });

      setResult(scanResult);
    } catch (reason) {
      setResult(null);
      setError(String(reason));
    } finally {
      setScanning(false);
    }
  }

  return (
    <main className="app-shell">
      <section className="app">
        <header>
          <div className="eyebrow">ARCHIVER</div>
          <h1>Move old files out of the way.</h1>
          <p className="intro">
            Choose what counts as old. Archiver shows you exactly what would
            move before anything happens.
          </p>
        </header>

        <section className="controls" aria-label="Archive settings">
          <div className="field">
            <label>Source</label>
            <button className="path-button" onClick={() => chooseFolder("source")}>
              <span className={source ? "" : "placeholder"}>
                {source || "Choose a folder or drive"}
              </span>
              <span className="choose">Choose</span>
            </button>
          </div>

          <div className="field">
            <label>Archive to</label>
            <button
              className="path-button"
              onClick={() => chooseFolder("destination")}
            >
              <span className={destination ? "" : "placeholder"}>
                {destination || "Choose an archive folder"}
              </span>
              <span className="choose">Choose</span>
            </button>
          </div>

          <div className="field cutoff-field">
            <label htmlFor="cutoff">Archive everything before</label>
            <input
              id="cutoff"
              type="number"
              min="1970"
              max="9999"
              value={cutoffYear}
              onChange={(event) => {
                setCutoffYear(event.currentTarget.value);
                setResult(null);
                setShowFiles(false);
                setSelectedYear(null);
              }}
            />
          </div>

          <button className="scan-button" disabled={!canScan || scanning} onClick={scan}>
            {scanning ? "Scanning…" : "Scan"}
          </button>
        </section>

        {error && <div className="error">{error}</div>}

        {result && (
          <section className="results">
            <div className="result-heading">
              <div>
                <div className="eyebrow">PREVIEW</div>
                <h2>{heading}</h2>
              </div>
              <div className="total">
                <strong>{formatBytes(result.totalSize)}</strong>
                <span>{result.totalFiles.toLocaleString()} files</span>
              </div>
            </div>

            {result.years.length > 0 && (
              <div className="year-list">
                {result.years.map((year) => (
                  <button
                    className={`year-row ${
                      selectedYear === year.year ? "selected" : ""
                    }`}
                    key={year.year}
                    onClick={() => {
                      setSelectedYear(year.year);
                      setShowFiles(true);
                    }}
                  >
                    <strong>{year.year}</strong>
                    <span>{year.fileCount.toLocaleString()} files</span>
                    <span>{formatBytes(year.totalSize)}</span>
                  </button>
                ))}
              </div>
            )}

            {result.skippedFiles > 0 && (
              <p className="skipped">
                {result.skippedFiles.toLocaleString()} inaccessible item
                {result.skippedFiles === 1 ? "" : "s"} skipped.
              </p>
            )}

            <div className="result-actions">
              <button
                className="secondary"
                disabled={result.totalFiles === 0}
                onClick={() => {
                  if (showFiles) {
                    setShowFiles(false);
                    setSelectedYear(null);
                  } else {
                    setSelectedYear(null);
                    setShowFiles(true);
                  }
                }}
              >
                {showFiles ? "Hide files" : "Preview all files"}
              </button>

              <button className="archive-button" disabled>
                Archive {formatBytes(result.totalSize)}
              </button>
            </div>

            {showFiles && (
              <section className="file-preview">
                <div className="file-preview-heading">
                  <div>
                    <strong>
                      {selectedYear === null
                        ? "All files"
                        : `${selectedYear} files`}
                    </strong>
                    <span>
                      {result.files
                        .filter(
                          (file) =>
                            selectedYear === null ||
                            file.year === selectedYear,
                        )
                        .length.toLocaleString()}{" "}
                      files
                    </span>
                  </div>

                  <button
                    className="close-preview"
                    onClick={() => {
                      setShowFiles(false);
                      setSelectedYear(null);
                    }}
                  >
                    Close
                  </button>
                </div>

                <div className="file-list">
                  {result.files
                    .filter(
                      (file) =>
                        selectedYear === null ||
                        file.year === selectedYear,
                    )
                    .map((file) => (
                      <div
                        className="file-row"
                        key={`${file.modifiedMs}-${file.relativePath}`}
                      >
                        <div>
                          <strong>{file.name}</strong>
                          <span>{file.relativePath}</span>
                        </div>
                        <span>
                          {new Date(file.modifiedMs).toLocaleDateString()}
                        </span>
                        <span>{formatBytes(file.size)}</span>
                      </div>
                    ))}
                </div>
              </section>
            )}
          </section>
        )}

        {!result && (
          <div className="safety-note">
            Preview only. This version cannot move, rename or delete files.
          </div>
        )}
      </section>
    </main>
  );
}

export default App;
