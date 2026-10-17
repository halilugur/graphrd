package com.example.personnel.model.dto;

/** One JSON shape for every error the API answers with. */
public record ApiError(String status, String message) {
}
