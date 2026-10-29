import React, { useState } from "react";

export interface GeneratorModalProps {
  isOpen: boolean;
  onClose: () => void;
  onGenerate?: (manifest: string) => void;
  title?: string;
  children?: React.ReactNode;
}

const BASE_BUTTON_CLASSES =
  "inline-flex items-center justify-center rounded-md font-medium transition-colors focus:outline-none focus:ring-2 focus:ring-offset-2";

const PRIMARY_BUTTON_CLASSES = `${BASE_BUTTON_CLASSES} px-4 py-2 bg-blue-600 text-white hover:bg-blue-700 focus:ring-blue-500 disabled:opacity-50 disabled:cursor-not-allowed`;

const SECONDARY_BUTTON_CLASSES = `${BASE_BUTTON_CLASSES} px-4 py-2 border border-gray-300 bg-white text-gray-700 hover:bg-gray-50 focus:ring-gray-400`;

export const GeneratorModal: React.FC <GeneratorModalProps> = ({
  isOpen,
  onClose,
  onGenerate,
  title = "Manifest Generator",
  children,
}) => {
  const [manifest, setManifest] = useState<string>("");
  const [isGenerating, setIsGenerating] = useState<boolean>(false);

  if (!isOpen) {
    return null;
  }

  const handleGenerate = async () => {
    setIsGenerating(true);
    try {
      const generated = JSON.stringify(
        {
          name: "manifest",
          generatedAt: new Date().toISOString(),
        },
        null,
        2,
      );
      setManifest(generated);
      onGenerate?.(generated);
    } finally {
      setIsGenerating(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="generator-modal-title"
    >
      <div className="w-full max-w-lg rounded-lg bg-white p-6 shadow-xl">
        <h2 id="generator-modal-title" className="mb-4 text-lg font-semibold text-gray-900">
          {title}
        </h2>

        <div className="mb-4">{children}</div>

        {manifest && (
          <pre className="mb-4 max-h-60 overflow-auto rounded-md bg-gray-100 p-3 text-sm text-gray-800">
            {manifest}
          </pre>
        )}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            className={SECONDARY_BUTTON_CLASSES}
            onClick={onClose}
            disabled={isGenerating}
          >
            Close
          </button>
          <button
            type="button"
            className={PRIMARY_BUTTON_CLASSES}
            onClick={handleGenerate}
            disabled={isGenerating}
          >
            {isGenerating ? "Generating..." : "Generate"}
          </button>
        </div>
      </div>
    </div>
  );
};

export default GeneratorModal;
