package com.example.personnel.model.dto;

/** The request body of {@code POST /companies}. */
public record CompanyIn(String name, String industry) {
}
