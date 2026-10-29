import React, { useEffect, useState } from "react";
import "./form_error.css";

export interface FormErrorProps {
    message: string | Error;
    kind?: "api" | "network";
    title?: string;
    onRetry?: () => void | Promise<void>;
    autoDismissMs?: number;
    onDismiss?: () => void;
    className?: string;
}

export const FormError: React.FC<FormErrorProps> = ({
    message,
    kind = "api",
    title,
    onRetry,
    autoDismissMs,
    onDismiss,
    className,
}) => {
    const [isVisible, setIsVisible] = useState(true);
    const errorMessage = message instanceof Error ? message.message : message;

    useEffect(() => {
        setIsVisible(true);

        if (autoDismissMs === undefined || autoDismissMs <= 0) return;

        const timeoutId = window.setTimeout(() => {
            setIsVisible(false);
            onDismiss?.();
        }, autoDismissMs);

        return () => window.clearTimeout(timeoutId);
    }, [autoDismissMs, errorMessage, onDismiss]);

    const dismiss = () => {
        setIsVisible(false);
        onDismiss?.();
    };

    if (!isVisible || !errorMessage) return null;

    const rootClassName = ["form-level-error", className]
        .filter(Boolean)
        .join(" ");

    return (
        <div className={rootClassName} role="alert" aria-live="assertive" aria-atomic="true">
            <span className="form-level-error__icon" aria-hidden="true">!</span>
            <div className="form-level-error__content">
                <p className="form-level-error__title">
                    {title ?? (kind === "network" ? "Network error" : "Submission failed")}
                </p>
                <p className="form-level-error__message">{errorMessage}</p>
            </div>
            <div className="form-level-error__actions">
                {onRetry && (
                    <button
                        className="form-level-error__retry"
                        type="button"
                        onClick={() => { void onRetry(); }}
                    >
                        Try again
                    </button>
                )}
                <button
                    className="form-level-error__dismiss"
                    type="button"
                    aria-label="Dismiss error"
                    onClick={dismiss}
                >
                    &times;
                </button>
            </div>
        </div>
    );
};

export default FormError;