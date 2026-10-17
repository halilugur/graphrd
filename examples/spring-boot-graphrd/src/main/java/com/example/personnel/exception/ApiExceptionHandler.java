package com.example.personnel.exception;

import java.util.NoSuchElementException;

import com.example.personnel.model.dto.ApiError;
import org.springframework.http.HttpStatus;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.ExceptionHandler;
import org.springframework.web.bind.annotation.RestControllerAdvice;


/** One JSON shape for every error the API answers with. */
@RestControllerAdvice
public class ApiExceptionHandler {

    @ExceptionHandler(NoSuchElementException.class)
    ResponseEntity<ApiError> notFound(NoSuchElementException e) {
        return answer(HttpStatus.NOT_FOUND, e.getMessage());
    }

    @ExceptionHandler(IllegalArgumentException.class)
    ResponseEntity<ApiError> badRequest(IllegalArgumentException e) {
        return answer(HttpStatus.BAD_REQUEST, e.getMessage());
    }

    private ResponseEntity<ApiError> answer(HttpStatus status, String message) {
        return ResponseEntity.status(status).body(new ApiError(status.name(), message));
    }
}
