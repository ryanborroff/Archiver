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

type PlannedFile = {
  source: string;
  destination: string;
  relativePath: string;
  year: number;
  size: number;
  status: "ready" | "conflict" | "changed" | "missing";
  detail: string | null;
};

type ArchivePlan = {
  files: PlannedFile[];
  readyFiles: number;
  readySize: number;
  conflicts: number;
  changedFiles: number;
  missingFiles: number;
};

type ExecutionItem = {
  relativePath: string;
  destination: string;
  status: "archived" | "sourceRetained" | "failed";
  detail: string | null;
};

type ExecutionResult = {
  items: ExecutionItem[];
  archivedFiles: number;
  archivedSize: number;
  sourceRetainedFiles: number;
  failedFiles: number;
};

type ArchiveExecutionResponse = {
  result: ExecutionResult;
  journalPath: string;
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
  const [plan, setPlan] = useState<ArchivePlan | null>(null);
  const [planning, setPlanning] = useState(false);
  const [confirmingArchive, setConfirmingArchive] = useState(false);
  const [archiving, setArchiving] = useState(false);
  const [execution, setExecution] =
    useState<ArchiveExecutionResponse | null>(null);

  const cutoff = Number(cutoffYear);
  const canScan =
    source.length > 0 &&
    destination.length > 0 &&
    Number.isInteger(cutoff) &&
    cutoff >= 1970 &&
    !archiving;

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
    setPlan(null);
    setExecution(null);
    setConfirmingArchive(false);
    setShowFiles(false);
    setSelectedYear(null);
    setError("");
  }

  async function scan() {
    if (!canScan) return;

    setScanning(true);
    setError("");
    setPlan(null);
    setExecution(null);
    setConfirmingArchive(false);
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

  async function reviewArchive() {
    if (!result || !source || !destination) {
      setError("Cannot review archive: scan data or folder selection is missing.");
      return;
    }

    setPlanning(true);
    setError("");
    setExecution(null);
    setConfirmingArchive(false);

    try {
      const archivePlan = await invoke<ArchivePlan>("plan_archive", {
        source,
        archiveDestination: destination,
        files: result.files,
      });

      setPlan(archivePlan);
    } catch (reason) {
      setPlan(null);
      setError(`Could not review archive: ${String(reason)}`);
    } finally {
      setPlanning(false);
    }
  }

  async function executeArchive() {
    if (!result || !plan || !source || !destination) return;

    const planIsSafe =
      plan.readyFiles === result.totalFiles &&
      plan.conflicts === 0 &&
      plan.changedFiles === 0 &&
      plan.missingFiles === 0;

    if (!planIsSafe) {
      setError(
        "The archive plan is no longer fully ready. Review the archive again before continuing.",
      );
      setConfirmingArchive(false);
      return;
    }

    setArchiving(true);
    setError("");
    setExecution(null);

    let response: ArchiveExecutionResponse;

    try {
      response = await invoke<ArchiveExecutionResponse>(
        "execute_archive",
        {
          source,
          archiveDestination: destination,
          files: result.files,
        },
      );
    } catch (reason) {
      setError(`Archive did not complete: ${String(reason)}`);
      setConfirmingArchive(false);
      setArchiving(false);
      return;
    }

    // The archive operation itself has completed successfully at this point.
    // Record that result before attempting the non-destructive UI refresh.
    setExecution(response);
    setPlan(null);
    setConfirmingArchive(false);
    setShowFiles(false);
    setSelectedYear(null);
    setArchiving(false);

    try {
      const refreshed = await invoke<ScanResult>("scan_archive", {
        source,
        archiveDestination: destination,
        cutoffYear: cutoff,
      });

      setResult(refreshed);
    } catch (reason) {
      setResult(null);
      setError(
        `Archive completed successfully, but the source could not be refreshed: ${String(reason)}`,
      );
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
            <button
              className="path-button"
              disabled={archiving}
              onClick={() => chooseFolder("source")}
            >
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
              disabled={archiving}
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
              disabled={archiving}
              onChange={(event) => {
                setCutoffYear(event.currentTarget.value);
                setResult(null);
                setPlan(null);
                setExecution(null);
                setConfirmingArchive(false);
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

              <button
                className="archive-button"
                disabled={result.totalFiles === 0 || planning}
                onClick={reviewArchive}
              >
                {planning ? "Reviewing…" : "Review archive"}
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

        {plan && (
          <section className="plan-panel">
            <div className="plan-heading">
              <div>
                <div className="eyebrow">ARCHIVE PLAN</div>
                <h2>
                  {plan.conflicts === 0 &&
                  plan.changedFiles === 0 &&
                  plan.missingFiles === 0
                    ? "Ready for review"
                    : "Needs attention"}
                </h2>
              </div>

              <div className="total">
                <strong>{formatBytes(plan.readySize)}</strong>
                <span>{plan.readyFiles.toLocaleString()} ready</span>
              </div>
            </div>

            <div className="plan-summary">
              <div>
                <strong>{plan.readyFiles.toLocaleString()}</strong>
                <span>Ready</span>
              </div>
              <div>
                <strong>{plan.conflicts.toLocaleString()}</strong>
                <span>Conflicts</span>
              </div>
              <div>
                <strong>{plan.changedFiles.toLocaleString()}</strong>
                <span>Changed</span>
              </div>
              <div>
                <strong>{plan.missingFiles.toLocaleString()}</strong>
                <span>Missing</span>
              </div>
            </div>

            <div className="plan-list">
              {plan.files.map((file) => (
                <div
                  className={`plan-row plan-${file.status}`}
                  key={`${file.source}-${file.destination}`}
                >
                  <div className="plan-paths">
                    <strong>{file.relativePath}</strong>
                    <span>{file.source}</span>
                    <span>→ {file.destination}</span>
                    {file.detail && (
                      <span className="plan-detail">{file.detail}</span>
                    )}
                  </div>

                  <div className="plan-meta">
                    <span>{file.status}</span>
                    <span>{formatBytes(file.size)}</span>
                  </div>
                </div>
              ))}
            </div>

            <div className="plan-footer">
              {!confirmingArchive ? (
                <>
                  <span>
                    Nothing has moved yet. Archive only when this plan looks
                    right.
                  </span>
                  <button
                    className="archive-button"
                    disabled={
                      archiving ||
                      plan.readyFiles === 0 ||
                      plan.conflicts > 0 ||
                      plan.changedFiles > 0 ||
                      plan.missingFiles > 0 ||
                      !result ||
                      plan.readyFiles !== result.totalFiles
                    }
                    onClick={() => setConfirmingArchive(true)}
                  >
                    Archive files
                  </button>
                </>
              ) : (
                <div className="archive-confirmation">
                  <div>
                    <strong>
                      Archive {plan.readyFiles.toLocaleString()} files?
                    </strong>
                    <span>
                      {formatBytes(plan.readySize)} will be copied, verified,
                      then removed from the source.
                    </span>
                  </div>

                  <div className="archive-confirmation-actions">
                    <button
                      className="secondary"
                      disabled={archiving}
                      onClick={() => setConfirmingArchive(false)}
                    >
                      Cancel
                    </button>
                    <button
                      className="archive-button"
                      disabled={archiving}
                      onClick={executeArchive}
                    >
                      {archiving ? "Archiving…" : "Confirm archive"}
                    </button>
                  </div>
                </div>
              )}
            </div>
          </section>
        )}

        {execution && (
          <section className="plan-panel execution-panel">
            <div className="plan-heading">
              <div>
                <div className="eyebrow">ARCHIVE COMPLETE</div>
                <h2>
                  {execution.result.failedFiles === 0 &&
                  execution.result.sourceRetainedFiles === 0
                    ? "Archive complete"
                    : "Archive completed with attention needed"}
                </h2>
              </div>

              <div className="total">
                <strong>
                  {formatBytes(execution.result.archivedSize)}
                </strong>
                <span>
                  {execution.result.archivedFiles.toLocaleString()} archived
                </span>
              </div>
            </div>

            {(execution.result.sourceRetainedFiles > 0 ||
              execution.result.failedFiles > 0) && (
              <p className="skipped">
                {execution.result.sourceRetainedFiles.toLocaleString()} source
                retained, {execution.result.failedFiles.toLocaleString()} failed.
              </p>
            )}

            <p className="skipped">
              Recovery record: {execution.journalPath}
            </p>
          </section>
        )}

        {!result && !execution && (
          <div className="safety-note">
            Choose a source and archive folder, then scan to preview what would
            move.
          </div>
        )}
      </section>
    </main>
  );
}

export default App;
